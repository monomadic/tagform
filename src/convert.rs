//! `tagform convert FILE...`: an MP4 or MOV as a Matroska file, headless
//! (DESIGN §9.7).
//!
//! The streams are copied, never re-encoded, and the tags are carried as
//! field values and written under the Matroska rules -- one title, one
//! spelling of each key -- and not as whatever a muxer makes of the
//! source's atoms. The new file is re-seated before it is handed over, so
//! it starts life with its metadata at the front and room to edit it.
//!
//! The source is only read and is never removed. A Matroska file cannot
//! hold everything an MP4 can, and what it cannot hold is listed and
//! refused unless `--lossy` says the loss is acceptable: a conversion that
//! quietly left the subtitles behind would be the silent loss the whole
//! write path is built to prevent.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::clone::{device_key, name, source_rows, untangle};
use crate::model::schema::claimed_atom_keys;
use crate::model::value::Value;
use crate::tags::mkv;
use crate::tags::plan::{self, atom_text};
use crate::tags::probe::{probe, FileTags};
use crate::tags::write::{self, WriteError};

pub const USAGE: &str = "\
Usage: tagform convert [OPTIONS] FILE...

Make a Matroska copy of each MP4 or MOV, beside it and under the same name
with .mkv. Streams are copied, not re-encoded. Tags are carried over. The
source is left as it is.

  --lossy          convert even when something cannot be carried; what is
                   left behind is still listed
  -n, --dry-run    say what would be carried and what would not; write nothing
  -h, --help       show this message

Carried: video and audio streams, chapters, every field that has a value,
and every other tag that is plain text.

Not carried, and so refused without --lossy: subtitle streams (an MP4's can
only be converted, not copied), timecode and timed-metadata tracks, cover
art, XMP tags no field claims, and reverse-DNS keys other than the
coordinates.

Exit: 0 every file converted, 1 a file failed or was refused, 2 bad
arguments, 3 the only failures were lack of space.";

struct Opts {
    files: Vec<PathBuf>,
    lossy: bool,
    dry_run: bool,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Opts>> {
    let (mut lossy, mut dry_run, mut rest) = (false, false, false);
    let mut files: Vec<PathBuf> = Vec::new();
    for a in args {
        if rest {
            files.push(a.into());
            continue;
        }
        match a.as_str() {
            "--" => rest = true,
            "--lossy" => lossy = true,
            "-n" | "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            s if s.starts_with('-') => bail!("unknown option: {s}\n\n{USAGE}"),
            _ => files.push(a.into()),
        }
    }
    if files.is_empty() {
        bail!("convert needs at least one file\n\n{USAGE}");
    }
    Ok(Some(Opts {
        files,
        lossy,
        dry_run,
    }))
}

/// The tags a Matroska file has no way to hold, one line each.
fn lost_tags(src: &FileTags, rows: &BTreeMap<String, Value>) -> Vec<String> {
    let claimed = claimed_atom_keys();
    let mut lost: Vec<String> = src
        .atoms
        .keys()
        .filter(|k| device_key(k) && !claimed.contains(&k.as_str()))
        .map(|k| format!("key {k}"))
        .collect();
    lost.extend(
        rows.keys()
            .filter_map(|k| k.strip_prefix("xmp:"))
            .map(|t| format!("XMP tag {t}")),
    );
    lost
}

#[derive(Debug)]
pub struct Outcome {
    pub dest: PathBuf,
    pub carried: Vec<String>,
    pub lost: Vec<String>,
}

/// Convert one file, or with `dry_run` only work out what would happen.
pub fn convert(src: &Path, lossy: bool, dry_run: bool) -> Result<Outcome, WriteError> {
    let fail = |e: anyhow::Error| WriteError::Failed(e);
    if mkv::is_matroska(src) {
        return Err(fail(anyhow::anyhow!("already a Matroska file")));
    }
    let dest = src.with_extension("mkv");
    if dest.exists() {
        return Err(fail(anyhow::anyhow!("{} already exists", dest.display())));
    }
    let tags = probe(src).map_err(fail)?;
    let all = source_rows(&tags);
    let rows = untangle(all.clone(), &all);
    let streams = write::carried(src);
    if streams.kept.is_empty() {
        return Err(fail(anyhow::anyhow!("no video or audio stream to carry")));
    }

    let mut lost = streams.lost.clone();
    lost.extend(lost_tags(&tags, &rows));
    // Planned against a file with no tags at all, which is what ffmpeg is
    // about to make: every row is a change.
    let blank = FileTags {
        path: dest.clone(),
        atoms: BTreeMap::new(),
        xmp: BTreeMap::new(),
    };
    let staged: BTreeMap<String, Value> = rows
        .iter()
        .filter(|(k, v)| !k.starts_with("xmp:") && !atom_text(v).is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let (atoms, xmp) = plan::keys(&blank, &staged);
    let xmp: Vec<_> = xmp
        .into_iter()
        .filter(|(t, _)| mkv::carries_xmp(t))
        .collect();

    let outcome = Outcome {
        dest: dest.clone(),
        carried: staged.keys().map(|k| name(k)).collect(),
        lost,
    };
    if !outcome.lost.is_empty() && !lossy {
        let list = outcome.lost.join("\n  ");
        return Err(fail(anyhow::anyhow!(
            "a Matroska file cannot hold everything this one does; --lossy converts without:\n  {list}"
        )));
    }
    if dry_run {
        return Ok(outcome);
    }
    write::to_matroska(src, &dest, &streams.kept, &atoms, &xmp, &mut |_| {})?;
    Ok(outcome)
}

pub fn run(args: impl IntoIterator<Item = String>) -> Result<i32> {
    let Some(opts) = parse(args)? else {
        return Ok(0);
    };
    let (mut failed, mut no_space) = (false, false);
    for src in &opts.files {
        let shown = src.display();
        match convert(src, opts.lossy, opts.dry_run) {
            Ok(o) => {
                let verb = if opts.dry_run { "would make" } else { "made" };
                println!("{shown}: {verb} {}", o.dest.display());
                println!("  carried: {}", o.carried.join(", "));
                for l in &o.lost {
                    println!("  left behind: {l}");
                }
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
