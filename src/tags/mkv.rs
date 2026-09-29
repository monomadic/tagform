//! Matroska, through `fastmkv` (DESIGN §9.6).
//!
//! A separate backend rather than a fourth route through the MP4 ones: an
//! `.mkv` has no atoms and no XMP, and every MP4 writer here would treat it
//! as one. What this module does is translate. The form and the planner
//! speak lower-cased container keys; a Matroska file holds named tags in
//! whatever case it was given, plus a title that is not a tag at all.
//!
//! The rules, each of them measured against ffprobe, mpv, mediainfo and VLC
//! rather than read off the specification:
//!
//! - Names differing only in case are one key. The last in the file is the
//!   one shown, as ffprobe shows it, and the others are removed on write:
//!   mpv joins them and mediainfo lists them, so leaving them is leaving
//!   three readers with three answers.
//! - The title is `Info\Title`. A `TITLE` tag is read when there is no
//!   other, and removed on write, because a file with both shows its title
//!   twice in mpv even when they agree.
//! - A key is written under the name the file already uses for it, and in
//!   upper case when it is new, which is what ffmpeg does to an MP4's keys
//!   when it converts one.

use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::path::Path;

use fastmkv::{Kind, Mkv, Padding};

use crate::model::schema::JUNK_KEYS;
use crate::model::value::Value;

/// The fields whose only home in an MP4 is XMP, and the tag each has here.
/// Read back under the XMP name, so the schema's lookup needs no second
/// path for them.
const XMP_NAMES: &[(&str, &str)] = &[
    ("XMP-iptcExt:LocationCreatedSublocation", "LOCATION_PLACE"),
    ("XMP-iptcExt:LocationCreatedCity", "LOCATION_CITY"),
    ("XMP-iptcExt:LocationCreatedProvinceState", "LOCATION_STATE"),
    ("XMP-iptcExt:LocationCreatedCountryName", "LOCATION_COUNTRY"),
    ("XMP-xmpMM:PreservedFileName", "PRESERVED_NAME"),
];

/// By the EBML magic, not the extension, for the same reason the MP4
/// backend is chosen from file contents (DESIGN §9.2).
pub fn is_matroska(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == [0x1A, 0x45, 0xDF, 0xA3]
}

pub struct Read {
    pub atoms: BTreeMap<String, Value>,
    pub xmp: BTreeMap<String, Value>,
}

fn project<'a>(title: Option<String>, tags: impl Iterator<Item = (&'a str, &'a str)>) -> Read {
    let mut atoms = BTreeMap::new();
    let mut xmp = BTreeMap::new();
    for (name, value) in tags {
        if let Some((tag, _)) = XMP_NAMES.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
            xmp.insert(tag.to_string(), Value::text(value));
            continue;
        }
        let key = name.to_ascii_lowercase();
        if !JUNK_KEYS.contains(&key.as_str()) {
            // Plain insertion, so the last of a repeated name wins.
            atoms.insert(key, Value::text(value));
        }
    }
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        atoms.insert("title".into(), Value::text(t));
    }
    Read { atoms, xmp }
}

/// What the form shows. From the full walk whenever the file allows one:
/// the quick reader does not visit what lies past the clusters, and a field
/// that looked empty because its tag was not found would be filled in and
/// the real value written over. A file the full walk refuses cannot be
/// written either, so the quick reader is safe for it.
pub fn read(path: &Path) -> Result<Read> {
    match fastmkv::open(path) {
        Ok(mkv) => Ok(project(mkv.title(), mkv.global())),
        Err(fastmkv::Error::Refused(_)) => {
            let m = fastmkv::read(path).map_err(|e| anyhow!("{e}"))?;
            Ok(project(m.title.clone(), m.global()))
        }
        Err(e) => bail!("{e}"),
    }
}

/// How a plan will be carried out, decided without writing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The tags fit where they are.
    InPlace,
    /// They do not, and go to the end of the file.
    Append,
    /// A copy with the metadata at the front and room after it.
    Reseat,
    /// `fastmkv` will not edit this file; the write will say why.
    Refused,
}

impl Route {
    pub fn why(self) -> &'static str {
        match self {
            Route::InPlace => "the tags fit in the room the file has for them",
            Route::Append => "no room at the front, so the tags go to the end of the file",
            Route::Reseat => "copies the file with its tags at the front and room after them",
            Route::Refused => "not a file the Matroska writer can edit; the write will say why",
        }
    }
}

/// Every spelling of `key` among the global tags, in file order.
fn spellings(mkv: &Mkv, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in mkv.global() {
        if name.eq_ignore_ascii_case(key) && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// One key, one value, one spelling. An empty value removes the key.
fn put(mkv: &mut Mkv, key: &str, new_name: &str, value: &str) -> fastmkv::Result<()> {
    let mut names = spellings(mkv, key);
    let name = names.pop().unwrap_or_else(|| new_name.to_string());
    for other in names {
        mkv.remove(&other)?;
    }
    match value.is_empty() {
        true => mkv.remove(&name),
        false => mkv.set(&name, value),
    }
}

fn stage(
    mkv: &mut Mkv,
    atoms: &[(String, String)],
    xmp: &[(String, Vec<String>)],
) -> fastmkv::Result<()> {
    for (key, value) in atoms {
        if key == "title" {
            mkv.set_title((!value.is_empty()).then_some(value.as_str()))?;
            for name in spellings(mkv, "title") {
                mkv.remove(&name)?;
            }
            continue;
        }
        put(mkv, key, &key.to_uppercase(), value)?;
    }
    for (tag, values) in xmp {
        // A tag with no name here has no home in this container. The
        // planner only stages the ones above, so this is not reached.
        if let Some((_, name)) = XMP_NAMES.iter().find(|(t, _)| t == tag) {
            put(mkv, name, name, &values.join(", "))?;
        }
    }
    Ok(())
}

enum Job {
    Nothing,
    Update(fastmkv::Plan),
    Reseat,
}

fn decide(mkv: &Mkv, pad: bool) -> fastmkv::Result<(Job, Route)> {
    let seat = mkv.seating();
    match mkv.plan() {
        Ok(p) if p.is_empty() => Ok((Job::Nothing, Route::InPlace)),
        Ok(p) => {
            let appends = p.new_len != p.old_len;
            if pad && (appends || !seat.front || seat.tags_padding == 0) {
                Ok((Job::Reseat, Route::Reseat))
            } else if appends {
                Ok((Job::Update(p), Route::Append))
            } else {
                Ok((Job::Update(p), Route::InPlace))
            }
        }
        // Each of these is a file with no room where room is needed, which
        // is what a re-seat makes. A title that does not fit is the one
        // that cannot be had any other way, so it re-seats with the toggle
        // off as well: the alternative is refusing the edit.
        Err(e)
            if matches!(
                e.kind(),
                Some(Kind::NeedsReseat | Kind::NoRoom | Kind::SeekHead)
            ) =>
        {
            Ok((Job::Reseat, Route::Reseat))
        }
        Err(e) => Err(e),
    }
}

pub fn route(
    path: &Path,
    atoms: &[(String, String)],
    xmp: &[(String, Vec<String>)],
    pad: bool,
) -> Route {
    let decided = fastmkv::open(path).and_then(|mut mkv| {
        stage(&mut mkv, atoms, xmp)?;
        decide(&mkv, pad)
    });
    decided.map_or(Route::Refused, |(_, route)| route)
}

/// Write the plan into `tmp`, which must not exist. The original is only
/// read. Returns what was done, or `None` when the file already says
/// everything the plan asks and there is nothing to swap in.
pub fn write(
    path: &Path,
    tmp: &Path,
    atoms: &[(String, String)],
    xmp: &[(String, Vec<String>)],
    pad: bool,
) -> Result<Option<Route>> {
    let mut mkv = fastmkv::open(path).map_err(|e| anyhow!("{e}"))?;
    stage(&mut mkv, atoms, xmp).map_err(|e| anyhow!("{e}"))?;
    let (job, route) = decide(&mkv, pad).map_err(|e| anyhow!("{e}"))?;
    match job {
        Job::Nothing => return Ok(None),
        Job::Reseat => mkv
            .reseat(tmp, Padding::default())
            .map_err(|e| anyhow!("{e}"))?,
        Job::Update(plan) => {
            // A clone on APFS, so this is not the cost of the file.
            std::fs::copy(path, tmp)?;
            plan.apply_to(tmp).map_err(|e| anyhow!("{e}"))?;
        }
    }
    // Whatever was written has to be a file this writer would accept.
    fastmkv::open(tmp).map_err(|e| anyhow!("the result does not pass its own check: {e}"))?;
    Ok(Some(route))
}

/// Stage the plan and write a re-seated copy to `tmp`, whatever room the
/// file has. For a file that has just been made, where the copy is the
/// point: it leaves with its tags at the front and padding after them.
pub fn seat(
    path: &Path,
    tmp: &Path,
    atoms: &[(String, String)],
    xmp: &[(String, Vec<String>)],
) -> Result<()> {
    let mut mkv = fastmkv::open(path).map_err(|e| anyhow!("{e}"))?;
    stage(&mut mkv, atoms, xmp).map_err(|e| anyhow!("{e}"))?;
    mkv.reseat(tmp, Padding::default())
        .map_err(|e| anyhow!("{e}"))
}

/// Whether a plan's XMP tag has a name in this container.
pub fn carries_xmp(tag: &str) -> bool {
    XMP_NAMES.iter().any(|(t, _)| *t == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: Option<&Value>) -> Option<String> {
        v.map(|v| match v {
            Value::Text(s) => s.clone(),
            Value::List(l) => l.join(", "),
        })
    }

    #[test]
    fn the_last_spelling_wins_and_junk_is_dropped() {
        let tags = [
            ("Artist", "first"),
            ("ARTIST", "second"),
            ("ENCODER", "Lavf"),
            ("artist", "third"),
            ("LOCATION_CITY", "Lisbon"),
        ];
        let r = project(None, tags.into_iter());
        assert_eq!(text(r.atoms.get("artist")).as_deref(), Some("third"));
        assert!(!r.atoms.contains_key("encoder"));
        assert!(
            !r.atoms.contains_key("location_city"),
            "it is not also a custom key"
        );
        assert_eq!(
            text(r.xmp.get("XMP-iptcExt:LocationCreatedCity")).as_deref(),
            Some("Lisbon")
        );
    }

    #[test]
    fn the_title_in_info_is_the_title() {
        let r = project(
            Some("from Info".into()),
            [("TITLE", "from a tag")].into_iter(),
        );
        assert_eq!(text(r.atoms.get("title")).as_deref(), Some("from Info"));
        let r = project(None, [("TITLE", "from a tag")].into_iter());
        assert_eq!(text(r.atoms.get("title")).as_deref(), Some("from a tag"));
        let r = project(Some(String::new()), [("TITLE", "from a tag")].into_iter());
        assert_eq!(text(r.atoms.get("title")).as_deref(), Some("from a tag"));
    }

    /// Every field that lives only in XMP has a name here, or an edit to it
    /// on a Matroska file would be dropped without a word.
    #[test]
    fn every_xmp_only_field_has_a_home() {
        for f in crate::model::schema::FIELDS
            .iter()
            .filter(|f| f.mdta.is_empty())
        {
            let tag = f.xmp.first().expect("an XMP-only field has an XMP tag");
            assert!(
                XMP_NAMES.iter().any(|(t, _)| t == tag),
                "{} ({tag}) has no Matroska tag name",
                f.id
            );
        }
    }
}
