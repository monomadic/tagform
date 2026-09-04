//! Deciding what to write, and which tool should write it (DESIGN §9.1, §9.2).
//!
//! Nothing here touches disk. The plan is built, shown for confirmation, and
//! only then executed -- mp4-tui-tagger's staging model, which is the reason
//! that script is trustworthy.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::model::schema::{field_by_id, JUNK_KEYS};
use crate::model::value::Value;
use crate::tags::atoms::Layout;
use crate::tags::probe::FileTags;

/// exiftool's tag name for each container key, as declared in
/// `assets/tagform.exiftool.cfg`. Measured against a real file: a key missing
/// from here cannot be updated in place, which is what forces a remux.
pub const EXIFTOOL_KEY_NAMES: &[(&str, &str)] = &[
    ("title", "Title"),
    ("actors", "Actors"),
    ("artist", "Artist"),
    ("rating", "RatingStars"),
    ("description", "Description"),
    ("webpage_url", "WebpageUrl"),
    ("source_url", "SourceUrl"),
    ("purl", "Purl"),
    ("comment", "Comment"),
    ("original_url", "OriginalUrl"),
    ("channel", "Channel"),
    ("album_artist", "AlbumArtistK"),
    ("album", "Album"),
    ("keywords", "Keywords"),
    ("category", "Category"),
    ("genre", "Genre"),
    ("variant", "Variant"),
    // Kept: files tagged before the rename carry `type`, and an in-place
    // update of one has to be able to name it (DESIGN §3.4).
    ("type", "Type"),
    ("media_type", "MediaTypeK"),
    ("date", "DateK"),
    ("synopsis", "SynopsisK"),
    ("origin", "Origin"),
    ("location", "Location"),
    ("track", "TrackK"),
];

pub fn exiftool_name(key: &str) -> Option<&'static str> {
    EXIFTOOL_KEY_NAMES.iter().find(|(k, _)| *k == key).map(|(_, n)| *n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// In place: preserves XMP, the inode, and xattrs. Cannot add a key.
    Exiftool,
    /// Rebuilds the container and nothing else (DESIGN §9.5): adds keys, keeps
    /// XMP, and keeps every track the remux would destroy. Preferred over both
    /// remux paths wherever the file's layout is one it can handle.
    Native,
    /// Full remux. Adds keys correctly; destroys XMP.
    Ffmpeg,
    /// Remux, then put the XMP back from the snapshot taken at read time.
    TwoPass,
}

impl Writer {
    pub fn label(self) -> &'static str {
        match self {
            Writer::Exiftool => "in place",
            Writer::Native => "rewrite container",
            Writer::Ffmpeg => "remux",
            Writer::TwoPass => "remux + restore XMP",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilePlan {
    pub path: PathBuf,
    pub writer: Writer,
    /// Container key -> new value. An empty value deletes the key.
    pub atoms: Vec<(String, String)>,
    /// XMP tag -> values. Written whenever the file already carries that tag,
    /// because XMP wins on read: updating only the atom would leave the form
    /// showing the old value and look like the edit did nothing.
    pub xmp: Vec<(String, Vec<String>)>,
    pub faststart: bool,
    pub layout: Layout,
    pub why: &'static str,
}

impl FilePlan {
    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty() && self.xmp.is_empty()
    }
}

/// Render a value into the single string an mdta atom holds. Lists are
/// comma-joined, which is what yt-dlp's `%(cast)l` and `%(tags)l` produce.
pub fn atom_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::List(l) => l.join(", "),
    }
}

fn xmp_values(v: &Value) -> Vec<String> {
    match v {
        Value::Text(s) => vec![s.clone()],
        Value::List(l) => l.clone(),
    }
}

pub fn build(
    file: &FileTags,
    staged: &BTreeMap<String, Value>,
    want_faststart: bool,
) -> FilePlan {
    let mut atoms: Vec<(String, String)> = Vec::new();
    let mut xmp: Vec<(String, Vec<String>)> = Vec::new();

    for (row_key, value) in staged {
        // Unclaimed keys go back where they came from. An XMP tag written as an
        // atom of the same name would be a new key that shadows nothing and is
        // read by nobody.
        if let Some(custom) = row_key.strip_prefix("custom:") {
            atoms.push((custom.to_string(), atom_text(value)));
            continue;
        }
        if let Some(tag) = row_key.strip_prefix("xmp:") {
            xmp.push((tag.to_string(), xmp_values(value)));
            continue;
        }
        let Some(def) = field_by_id(row_key) else { continue };
        for k in def.mdta {
            atoms.push((k.to_string(), atom_text(value)));
        }
        // Normally only touch XMP the file already has: inventing it on a plain
        // download would make ffprobe and exiftool disagree about the same file
        // forever. The exception is a field whose only home *is* XMP -- the
        // place name and the rest of the IPTC location block -- where writing
        // nothing would silently discard the edit.
        let xmp_only = def.mdta.is_empty();
        let tag = if xmp_only {
            def.xmp.first().copied()
        } else {
            def.xmp.iter().copied().find(|t| file.xmp.contains_key(*t))
        };
        if let Some(tag) = tag {
            xmp.push((tag.to_string(), xmp_values(value)));
        }
    }
    atoms.sort();
    atoms.dedup();

    let layout = crate::tags::atoms::layout(&file.path);
    let adds_new_key = atoms.iter().any(|(k, _)| !file.atoms.contains_key(k));
    let unwritable_in_place = atoms.iter().any(|(k, _)| exiftool_name(k).is_none());
    let has_xmp = !file.xmp.is_empty();
    // Only a remux can move the moov atom.
    let needs_remux_for_faststart = want_faststart && !layout.is_faststart();

    // The native writer handles everything the remux was for, without the two
    // losses that made the remux a last resort. It declines some layouts
    // (DESIGN §9.5), and ffmpeg stays the fallback for those.
    let native = crate::tags::native::survey(&file.path).ok().flatten().is_some();

    let (writer, why) = if !adds_new_key && !unwritable_in_place && !needs_remux_for_faststart {
        (Writer::Exiftool, "no new keys; keeps XMP, inode and xattrs")
    } else if native {
        (
            Writer::Native,
            if adds_new_key {
                "adds a key by rewriting the container; keeps XMP and every track"
            } else {
                "rewrites the container; keeps XMP and every track"
            },
        )
    } else if has_xmp {
        (
            Writer::TwoPass,
            if adds_new_key {
                "adds a key, and a bare remux would destroy this file's XMP"
            } else {
                "faststart needs a remux, and that would destroy this file's XMP"
            },
        )
    } else if adds_new_key {
        (Writer::Ffmpeg, "adds a key an in-place write cannot")
    } else if unwritable_in_place {
        (Writer::Ffmpeg, "a key exiftool cannot write in place")
    } else {
        (Writer::Ffmpeg, "faststart needs a remux")
    };

    FilePlan {
        path: file.path.clone(),
        writer,
        atoms,
        xmp,
        faststart: want_faststart,
        layout,
        why,
    }
}

/// Muxer bookkeeping is cleared on every remux. With `-map_metadata 0` plus
/// `use_metadata_tags`, ffmpeg promotes these to real readable tags that then
/// accumulate on each rewrite (docs/CONTAINER.md §1.3).
pub fn junk_clears() -> Vec<(String, String)> {
    JUNK_KEYS.iter().map(|k| (k.to_string(), String::new())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::probe::FileTags;
    use std::path::Path;

    fn file(atoms: &[(&str, &str)], xmp: &[(&str, &str)]) -> FileTags {
        FileTags {
            path: PathBuf::from("/tmp/tagform-plan-test.mp4"),
            atoms: atoms.iter().map(|(k, v)| (k.to_string(), Value::text(*v))).collect(),
            xmp: xmp.iter().map(|(k, v)| (k.to_string(), Value::text(*v))).collect(),
        }
    }
    fn staged(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    /// The shipped config and the name table are two halves of one fact, in
    /// two files with no compiler between them. A disagreement does not fail
    /// here -- it fails at write time, on a real file, as
    /// `Sorry, Keys:X doesn't exist or isn't writable`.
    ///
    /// Only one direction is checkable: a key the config declares must carry
    /// the name the table uses. The reverse is not a rule, because seven keys
    /// (title, artist, description, comment, album, keywords, genre) are in
    /// exiftool's own Keys table and must *not* be redeclared.
    #[test]
    fn the_shipped_exiftool_config_agrees_with_the_name_table() {
        let cfg = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/tagform.exiftool.cfg"),
        )
        .expect("reading assets/tagform.exiftool.cfg");

        let mut declared = 0;
        for line in cfg.lines().map(str::trim) {
            // `key => { Name => 'Tag', Writable => 'string' },`
            let Some((key, rest)) = line.split_once("=>") else { continue };
            let key = key.trim();
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                continue;
            }
            let Some(name) = rest.split('\'').nth(1) else { continue };
            declared += 1;
            assert_eq!(
                exiftool_name(key),
                Some(name),
                "{key} is declared as {name:?} in tagform.exiftool.cfg, but \
                 EXIFTOOL_KEY_NAMES says {:?}",
                exiftool_name(key)
            );
        }
        assert!(declared > 10, "parsed only {declared} declarations; the config format must have changed");
    }

    #[test]
    fn every_schema_write_key_has_an_exiftool_name() {
        for f in crate::model::schema::FIELDS {
            for k in f.mdta {
                assert!(
                    exiftool_name(k).is_some(),
                    "{k} has no exiftool name, so it can never be written in place"
                );
            }
        }
    }

    #[test]
    fn updating_an_existing_key_goes_in_place() {
        let f = file(&[("title", "old")], &[]);
        let p = build(&f, &staged(&[("title", Value::text("new"))]), false);
        assert_eq!(p.writer, Writer::Exiftool);
        assert_eq!(p.atoms, vec![("title".to_string(), "new".to_string())]);
    }

    /// CONTAINER.md §3.2: exiftool writes a *new* key in a form ffprobe cannot
    /// read, so adding one has to go through a remux.
    #[test]
    fn adding_a_key_forces_a_remux() {
        let f = file(&[("title", "old")], &[]);
        let p = build(&f, &staged(&[("genre", Value::text("Media"))]), false);
        assert_eq!(p.writer, Writer::Ffmpeg);
    }

    /// The finding that reshaped the design: a remux annihilates XMP, so a file
    /// carrying any must never be remuxed without putting it back.
    #[test]
    fn adding_a_key_to_an_xmp_file_uses_two_passes() {
        let f = file(&[("title", "old")], &[("XMP-dc:Title", "t")]);
        let p = build(&f, &staged(&[("genre", Value::text("Media"))]), false);
        assert_eq!(p.writer, Writer::TwoPass);
    }

    /// An XMP file that only needs updates stays in place -- the safest path.
    #[test]
    fn updating_an_xmp_file_stays_in_place() {
        let f = file(&[("title", "old")], &[("XMP-dc:Title", "t")]);
        let p = build(&f, &staged(&[("title", Value::text("new"))]), false);
        assert_eq!(p.writer, Writer::Exiftool);
    }

    #[test]
    fn faststart_on_a_moov_at_end_file_forces_a_remux() {
        // The fixture path does not exist, so layout() is Inconclusive, which
        // is not FastStart -- the conservative answer, and the one that keeps
        // a requested faststart honest.
        let f = file(&[("title", "old")], &[]);
        let p = build(&f, &staged(&[("title", Value::text("new"))]), true);
        assert_eq!(p.writer, Writer::Ffmpeg);
    }

    /// XMP is only written where it already lives; a plain download must not
    /// sprout an XMP block that then shadows its atoms on every later read.
    #[test]
    fn xmp_is_updated_only_when_present() {
        let with = file(&[("actors", "a")], &[("XMP-iptcExt:PersonInImage", "Alice")]);
        let p = build(&with, &staged(&[("actors", Value::List(vec!["Bob".into()]))]), false);
        assert_eq!(p.xmp, vec![("XMP-iptcExt:PersonInImage".to_string(), vec!["Bob".to_string()])]);

        let without = file(&[("actors", "a")], &[]);
        let p2 = build(&without, &staged(&[("actors", Value::List(vec!["Bob".into()]))]), false);
        assert!(p2.xmp.is_empty());
    }

    /// The field was called `type` on disk until it was renamed to `variant`
    /// (DESIGN §3.4). Every file tagged before that carries the old key, so it
    /// has to keep reading -- and an edit has to leave behind the new one.
    #[test]
    fn a_file_carrying_the_old_type_key_still_reads_as_variant() {
        let def = crate::model::schema::field_by_id("variant").expect("variant field");
        // "Master" is also the old spelling of the value, so this exercises
        // both renames at once: old key, old value, current reading.
        let old = file(&[("type", "Master")], &[]);
        assert_eq!(old.lookup(def), Some(Value::text("Enhanced")));

        // And the new key wins where a file somehow carries both.
        let both = file(&[("type", "Clip"), ("variant", "Original")], &[]);
        assert_eq!(both.lookup(def), Some(Value::text("Original")));
    }

    #[test]
    fn editing_variant_writes_the_new_key_only() {
        let f = file(&[("type", "Clip")], &[]);
        let p = build(&f, &staged(&[("variant", Value::text("Master"))]), false);
        assert_eq!(p.atoms, vec![("variant".to_string(), "Master".to_string())]);
        // A new key on this file, so it cannot go in place.
        assert_eq!(p.writer, Writer::Ffmpeg);
    }

    /// One field, five keys -- the fan-out that is the reason for the tool.
    #[test]
    fn the_url_field_writes_every_alias() {
        let f = file(
            &[("webpage_url", "a"), ("source_url", "a"), ("purl", "a"), ("comment", "a"), ("original_url", "a")],
            &[],
        );
        let p = build(&f, &staged(&[("url", Value::text("https://x/y"))]), false);
        let keys: Vec<&str> = p.atoms.iter().map(|(k, _)| k.as_str()).collect();
        for expected in ["webpage_url", "source_url", "purl", "comment", "original_url"] {
            assert!(keys.contains(&expected), "missing {expected}");
        }
        assert_eq!(p.writer, Writer::Exiftool);
    }

    /// Location lives only in XMP, so an edit must create the tag rather than
    /// vanish because the file happened not to carry it yet.
    #[test]
    fn an_xmp_only_field_is_written_even_when_absent() {
        let f = file(&[("title", "t")], &[]);
        let p = build(&f, &staged(&[("location", Value::text("Berlin"))]), false);
        assert_eq!(
            p.xmp,
            vec![("XMP-iptcExt:LocationCreatedCity".to_string(), vec!["Berlin".to_string()])]
        );
        assert!(p.atoms.is_empty(), "a place name must never be written as an atom");
    }

    /// An unclaimed XMP tag edited in the Custom section goes back to XMP, not
    /// to an atom of the same name.
    #[test]
    fn an_unclaimed_xmp_row_writes_xmp() {
        let f = file(&[], &[("XMP-iptcExt:LocationCreatedCountryName", "Thailand")]);
        let p = build(
            &f,
            &staged(&[("xmp:XMP-iptcExt:LocationCreatedCountryName", Value::text("Germany"))]),
            false,
        );
        assert!(p.atoms.is_empty());
        assert_eq!(p.xmp[0].0, "XMP-iptcExt:LocationCreatedCountryName");
    }

    #[test]
    fn a_list_is_comma_joined_for_the_atom_but_stays_a_list_for_xmp() {
        let v = Value::List(vec!["Alice".into(), "Bob".into()]);
        assert_eq!(atom_text(&v), "Alice, Bob");
        assert_eq!(xmp_values(&v), vec!["Alice".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn junk_is_always_cleared_on_a_remux() {
        let clears = junk_clears();
        assert!(clears.iter().any(|(k, v)| k == "major_brand" && v.is_empty()));
        assert!(clears.iter().any(|(k, _)| k == "compatible_brands"));
    }

    #[test]
    fn custom_keys_pass_straight_through() {
        let f = file(&[("yt_dlp_id", "abc")], &[]);
        let p = build(&f, &staged(&[("custom:yt_dlp_id", Value::text("xyz"))]), false);
        assert_eq!(p.atoms, vec![("yt_dlp_id".to_string(), "xyz".to_string())]);
        // Not in the exiftool config, so it cannot go in place.
        assert_eq!(p.writer, Writer::Ffmpeg);
    }

    fn _unused(_: &Path) {}
}
