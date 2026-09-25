//! `tagform clone SOURCE TARGET...`: one file's tags onto others, headless
//! (DESIGN §10).
//!
//! For the scripts that derive one file from another. An interpolated or
//! re-encoded copy comes out of ffmpeg carrying at best the half of the tags
//! an `ilst` mux keeps (invariant 1), and none of the XMP. This puts them back.
//!
//! Nothing here writes. The source's values become staged edits on each
//! target, exactly as though they had been typed into the form, and go through
//! the same `plan::build` and `write::execute`: the backend is chosen from the
//! target's contents, and a target is replaced only by a verified result. A
//! clone is a form edit with the typing done by another file.
//!
//! Headless, so it never opens the terminal, and it reports with the exit
//! codes §10 gives `--apply`: a script has to be able to tell "no space",
//! which retrying cannot fix, from a failure it can.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::schema::{
    claimed_atom_keys, claimed_xmp_tags, field_by_id, footage_label, FIELDS,
};
use crate::model::value::Value;
use crate::tags::plan::{self, atom_text};
use crate::tags::probe::{probe, FileTags};
use crate::tags::write::{self, WriteError};

pub const USAGE: &str = "\
Usage: tagform clone [OPTIONS] SOURCE TARGET...

Copy SOURCE's tags onto each TARGET, through the same planned and verified
write the form uses. Headless: nothing is asked, the terminal is never opened.

  --only=FIELDS    copy only these, comma-separated: field ids or labels as
                   --print-schema lists them (title, tags, actors, url, ...),
                   or custom:KEY / xmp:TAG for a key no field claims
  -n, --dry-run    print each target's write plan and write nothing
  --no-faststart   do not move a target's moov to the front
  -h, --help       show this message

Without --only, every field the source has a value for is copied, and every
key no field claims except reverse-DNS atoms (com.apple.quicktime.*), which
describe the device that recorded the source rather than the work. A field
the source lacks is left alone on the target, never cleared.

Exit: 0 every target written or already matching, 1 a target failed,
2 bad arguments or an unreadable source, 3 the only failures were lack of
space.";

struct Opts {
    source: PathBuf,
    targets: Vec<PathBuf>,
    /// Row keys to copy, as `plan::build` takes them. `None` is everything.
    only: Option<Vec<String>>,
    dry_run: bool,
    faststart: bool,
}

/// Run the subcommand; `args` is everything after `clone`. Returns the exit
/// code. An `Err` is a bad invocation or an unreadable source, which main
/// reports as `2`; a target that fails is reported here and counted instead,
/// so one bad target never stops the rest.
pub fn run(args: impl IntoIterator<Item = String>) -> Result<i32> {
    let Some(opts) = parse(args)? else {
        return Ok(0);
    };

    let src = probe(&opts.source).context("reading the source")?;
    let same = |t: &PathBuf| match (t.canonicalize(), opts.source.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if let Some(t) = opts.targets.iter().find(|t| same(t)) {
        bail!("{} is the source; it cannot also be a target", t.display());
    }

    let all = source_rows(&src);
    let rows = match &opts.only {
        None => all.clone(),
        Some(want) => {
            for k in want.iter().filter(|k| !all.contains_key(*k)) {
                eprintln!(
                    "tagform: the source has no {}; left alone on every target",
                    name(k)
                );
            }
            all.iter()
                .filter(|(k, _)| want.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }
    };
    let rows = untangle(rows, &all);

    let (mut failed, mut no_space) = (false, false);
    for path in &opts.targets {
        let target = match probe(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("tagform: {}: {e:#}", path.display());
                failed = true;
                continue;
            }
        };
        let edits = changes(&target, &rows);
        let plan = plan::build(&target, &edits, opts.faststart);
        let shown = path.display();
        if plan.is_empty() {
            println!("{shown}: already matches");
            continue;
        }
        if opts.dry_run {
            println!(
                "{shown}: would write via {} ({})",
                plan.writer.label(),
                plan.why
            );
            for (k, v) in &plan.atoms {
                println!("  {k} = {v}");
            }
            for (k, v) in &plan.xmp {
                println!("  {k} = {}", v.join(", "));
            }
            continue;
        }
        match write::execute(&plan, &target.xmp, &mut |_| {}) {
            Ok(()) => {
                let names: Vec<String> = edits.keys().map(|k| name(k)).collect();
                println!("{shown}: {} · {}", names.join(", "), plan.writer.label());
            }
            Err(e) => {
                eprintln!("tagform: {shown}: {e}");
                match e {
                    WriteError::NoSpace { .. } => no_space = true,
                    WriteError::Failed(_) => failed = true,
                }
            }
        }
    }
    Ok(if failed {
        1
    } else if no_space {
        3
    } else {
        0
    })
}

/// `None` when `--help` was asked for and printed.
fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Opts>> {
    let mut it = args.into_iter();
    let mut only: Option<Vec<String>> = None;
    let (mut dry_run, mut faststart, mut rest) = (false, true, false);
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut add = |list: &str| -> Result<()> {
        let mut keys = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(resolve)
            .collect::<Result<Vec<_>>>()?;
        if keys.is_empty() {
            bail!("--only names no fields");
        }
        only.get_or_insert_with(Vec::new).append(&mut keys);
        Ok(())
    };
    while let Some(a) = it.next() {
        if rest {
            paths.push(a.into());
            continue;
        }
        match a.as_str() {
            "--" => rest = true,
            "--only" => add(&it.next().context("--only needs a list of fields")?)?,
            s if s.starts_with("--only=") => add(&s["--only=".len()..])?,
            "-n" | "--dry-run" => dry_run = true,
            "--no-faststart" => faststart = false,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            s if s.starts_with('-') => bail!("unknown option: {s}\n\n{USAGE}"),
            _ => paths.push(a.into()),
        }
    }
    if paths.len() < 2 {
        bail!("clone needs a source and at least one target\n\n{USAGE}");
    }
    let source = paths.remove(0);
    Ok(Some(Opts {
        source,
        targets: paths,
        only,
        dry_run,
        faststart,
    }))
}

/// A name from `--only` as the row key it stands for. A field answers to its
/// id or its label, in any case, so `--only="Title, Tags"` and
/// `--only=title,tags` are the same request.
fn resolve(name: &str) -> Result<String> {
    if let Some(key) = name.strip_prefix("custom:") {
        // Probed atom names are lower-cased, and so are the rows built from them.
        let key = key.to_ascii_lowercase();
        if device_key(&key) {
            bail!(
                "{name} is a reverse-DNS key; ffprobe reads those lower-cased and their typed \
                 values as text, so it cannot be copied faithfully"
            );
        }
        return Ok(format!("custom:{key}"));
    }
    if name.starts_with("xmp:") {
        return Ok(name.to_string());
    }
    let want = name.to_lowercase();
    FIELDS
        .iter()
        .find(|f| {
            f.id == want
                || f.label.to_lowercase() == want
                || footage_label(f.id).is_some_and(|l| l.to_lowercase() == want)
        })
        .map(|f| f.id.to_string())
        .with_context(|| {
            let ids: Vec<&str> = FIELDS.iter().map(|f| f.id).collect();
            format!("unknown field {name:?}; fields are {}", ids.join(", "))
        })
}

/// A reverse-DNS atom (`com.apple.quicktime.make`, `com.android.version`) is a
/// fact about the device that recorded the source, not about the work: a
/// Live Photo pairing identifier copied onto an interpolated copy pairs the
/// wrong file. Nor could it be copied faithfully if it were wanted -- ffprobe
/// lower-cases the name, and a key is case-sensitive, and reports a float
/// payload as text. The one such key the schema writes, the ISO 6709
/// coordinate, is claimed by a field and travels as that.
fn device_key(key: &str) -> bool {
    key.contains('.')
}

/// Everything on the source worth copying, as row keys: every field with a
/// value, then the keys no field claims (invariant 4 -- a clone that dropped
/// them would be exactly the loss that invariant forbids).
fn source_rows(src: &FileTags) -> BTreeMap<String, Value> {
    let mut rows = BTreeMap::new();
    for f in FIELDS {
        if let Some(v) = src.lookup(f) {
            rows.insert(f.id.to_string(), v);
        }
    }
    let atoms = claimed_atom_keys();
    for (k, v) in &src.atoms {
        if !atoms.contains(&k.as_str()) && !device_key(k) && !v.is_empty() {
            rows.insert(format!("custom:{k}"), v.clone());
        }
    }
    let xmp = claimed_xmp_tags();
    for (k, v) in &src.xmp {
        if !xmp.contains(&k.as_str()) && !v.is_empty() {
            rows.insert(format!("xmp:{k}"), v.clone());
        }
    }
    rows
}

/// Two fields can write one key: Actors fans out to `actors` *and* `artist`,
/// and Artist writes `artist` (DESIGN §17.4). A source whose Artist was edited
/// apart from its Actors staged as-is asks one key for two values, and the
/// write refuses it -- correctly, but then every clone of that source fails.
///
/// The source already says which value each key holds, so a key goes to the
/// field that writes it alone. A fan-out field that disagrees with one is
/// staged as its remaining keys, one custom row each: `actors` still gets the
/// actors, `artist` keeps the artist, and nothing is asked twice. `all` is
/// every field the source carries, so an Artist left out by `--only` still
/// owns its key and a copied Actors does not overwrite it.
fn untangle(
    mut rows: BTreeMap<String, Value>,
    all: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let mut defs: Vec<_> = all.keys().filter_map(|k| field_by_id(k)).collect();
    defs.sort_by_key(|d| d.mdta.len()); // stable: schema order within a length
    let mut owner: BTreeMap<&str, String> = BTreeMap::new();
    let mut tangled = Vec::new();
    for d in defs {
        let text = atom_text(&all[d.id]);
        if d.mdta
            .iter()
            .any(|k| owner.get(k).is_some_and(|t| *t != text))
        {
            tangled.push(d);
        } else {
            owner.extend(d.mdta.iter().map(|k| (*k, text.clone())));
        }
    }
    for d in tangled {
        let Some(v) = rows.remove(d.id) else { continue };
        for k in d.mdta.iter().filter(|k| !owner.contains_key(*k)) {
            rows.insert(format!("custom:{k}"), v.clone());
        }
    }
    rows
}

/// The rows that would change the target. Compared as the atom text they
/// would write, so a one-item XMP list and the same string read as equal, as
/// they are on disk.
fn changes(target: &FileTags, rows: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    rows.iter()
        .filter(|(k, v)| target.row(k).map(|t| atom_text(&t)) != Some(atom_text(v)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A row key as a person would name it: the field's label, or the key itself.
fn name(key: &str) -> String {
    field_by_id(key)
        .map(|f| f.label.to_string())
        .unwrap_or_else(|| key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(atoms: &[(&str, &str)], xmp: &[(&str, &str)]) -> FileTags {
        FileTags {
            path: PathBuf::from("/tmp/tagform-clone-test.mp4"),
            atoms: atoms
                .iter()
                .map(|(k, v)| (k.to_string(), Value::text(*v)))
                .collect(),
            xmp: xmp
                .iter()
                .map(|(k, v)| (k.to_string(), Value::text(*v)))
                .collect(),
        }
    }
    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_takes_ids_and_labels_in_any_case() {
        let o = parse(args(&[
            "--only=Title, tags",
            "--only",
            "People,URL",
            "a.mp4",
            "b.mp4",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(o.only.unwrap(), ["title", "tags", "actors", "url"]);
        assert_eq!(o.source, PathBuf::from("a.mp4"));
        assert_eq!(o.targets, [PathBuf::from("b.mp4")]);
    }

    #[test]
    fn an_unknown_field_is_refused_with_the_list() {
        let e = parse(args(&["--only=titel", "a.mp4", "b.mp4"]))
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("titel") && e.contains("title"), "{e}");
    }

    #[test]
    fn a_source_alone_is_not_a_clone() {
        assert!(parse(args(&["a.mp4"])).is_err());
    }

    #[test]
    fn device_keys_are_refused_by_name_and_skipped_by_default() {
        assert!(resolve("custom:com.apple.quicktime.make").is_err());
        let src = file(
            &[("com.apple.quicktime.make", "Apple"), ("yt_dlp_id", "abc")],
            &[],
        );
        let rows = source_rows(&src);
        assert!(rows.contains_key("custom:yt_dlp_id"));
        assert!(!rows.keys().any(|k| k.contains("quicktime")));
    }

    /// Every field the source has, and every unclaimed key, and nothing else.
    #[test]
    fn source_rows_are_fields_then_unclaimed_keys() {
        let src = file(
            &[
                ("title", "T"),
                ("keywords", "a, b"),
                ("sound_designer", "S"),
            ],
            &[
                ("XMP-iptcExt:LocationCreatedCity", "Makati"),
                ("XMP-exif:GPSLatitude", "14.5"),
            ],
        );
        let rows = source_rows(&src);
        let keys: Vec<&str> = rows.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "custom:sound_designer",
                "location",
                "tags",
                "title",
                "xmp:XMP-exif:GPSLatitude"
            ]
        );
        assert_eq!(rows["tags"], Value::List(vec!["a".into(), "b".into()]));
    }

    /// The collision the fixture suite pins as a write failure has to be
    /// resolved before it gets there, or every clone of such a file fails.
    #[test]
    fn a_separately_edited_artist_keeps_its_key() {
        let src = file(&[("actors", "One, Two"), ("artist", "Someone Else")], &[]);
        let all = source_rows(&src);
        let rows = untangle(all.clone(), &all);
        assert_eq!(rows.get("artist"), Some(&Value::text("Someone Else")));
        assert!(
            !rows.contains_key("actors"),
            "Actors would ask `artist` for a second value"
        );
        assert_eq!(
            rows.get("custom:actors"),
            Some(&Value::List(vec!["One".into(), "Two".into()]))
        );

        let p = plan::build(&file(&[], &[]), &rows, false);
        let artist: Vec<_> = p.atoms.iter().filter(|(k, _)| k == "artist").collect();
        assert_eq!(artist.len(), 1, "one key, one value: {:?}", p.atoms);
    }

    /// Actors copied alone still leaves the source's Artist its key.
    #[test]
    fn untangling_sees_fields_left_out_by_only() {
        let src = file(&[("actors", "One"), ("artist", "Someone Else")], &[]);
        let all = source_rows(&src);
        let only: BTreeMap<_, _> = all
            .iter()
            .filter(|(k, _)| *k == "actors")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let rows = untangle(only, &all);
        assert_eq!(rows.keys().collect::<Vec<_>>(), ["custom:actors"]);
    }

    /// Agreeing fields are left as fields: the usual yt-dlp file, where both
    /// keys hold the same cast list.
    #[test]
    fn agreeing_actors_and_artist_stay_fields() {
        let src = file(&[("actors", "One, Two"), ("artist", "One, Two")], &[]);
        let all = source_rows(&src);
        let rows = untangle(all.clone(), &all);
        assert!(rows.contains_key("actors") && rows.contains_key("artist"));
    }

    #[test]
    fn values_the_target_already_has_are_not_staged() {
        let src = file(&[("title", "T"), ("keywords", "a, b")], &[]);
        let target = file(&[("title", "T"), ("keywords", "a")], &[]);
        let edits = changes(&target, &source_rows(&src));
        assert_eq!(edits.keys().collect::<Vec<_>>(), ["tags"]);
    }
}
