//! Writing mdta tags by rebuilding only the container (DESIGN §9.5).
//!
//! The other two writers each destroy something. A remux cannot carry a `mebx`
//! timed-metadata track and takes XMP with it (docs/CONTAINER.md §6, §2); the
//! in-place writer keeps both but cannot add a key that ffprobe will read
//! (§3.2). That second limit turned out not to be a property of the format:
//! exiftool appends the key name to the file's own `keys` box while writing
//! the value into a `moov/meta` box of its own, so the key has no `ilst` item
//! behind it and ffprobe -- which pairs the two by index inside one box --
//! sees nothing (§8).
//!
//! So this writer keeps a key and its item together, and copies everything
//! else through untouched. `mdat` is never parsed, which is why tracks this
//! tool has no model of survive: nothing looks at them. XMP survives for the
//! same reason.

use anyhow::{bail, Context, Result};
use std::path::Path;

use mp4box::edit::{Command, Editor};

/// One `ilst` item together with the `keys` entry that names it. Splitting
/// these two is the bug the module exists to avoid, so the type does not let
/// them be apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// The item's payload verbatim -- its `data` boxes, with their type codes
    /// and locales. Kept as bytes rather than as a string because a file's own
    /// keys include floats and integers, and a rewrite that meant to touch a
    /// neighbouring string must not reinterpret them.
    pub payload: Vec<u8>,
}

impl Entry {
    /// A new text entry, in the one encoding `tagform` writes: UTF-8, no locale.
    pub fn text(name: &str, value: &str) -> Entry {
        let mut data = Vec::with_capacity(16 + value.len());
        data.extend_from_slice(&((16 + value.len()) as u32).to_be_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&1u32.to_be_bytes()); // well-known type: UTF-8
        data.extend_from_slice(&0u32.to_be_bytes()); // locale
        data.extend_from_slice(value.as_bytes());
        Entry { name: name.to_string(), payload: data }
    }

    /// The value, when it is text. `None` for the typed payloads this module
    /// carries through without interpreting.
    ///
    /// Only the tests read a payload back; the writer itself never needs to
    /// interpret one, which is the property that keeps typed values safe.
    #[cfg(test)]
    pub fn as_text(&self) -> Option<&str> {
        let d = &self.payload;
        if d.len() < 16 || &d[4..8] != b"data" || u32::from_be_bytes(d[8..12].try_into().ok()?) != 1 {
            return None;
        }
        std::str::from_utf8(&d[16..]).ok()
    }
}

/// Where a file keeps its mdta box, and what is in it.
#[derive(Debug, Clone)]
pub struct Survey {
    /// The path `mp4box` addresses the boxes by. Apple writes `moov/meta`;
    /// ffmpeg writes `moov/udta/meta` (§8).
    pub base: String,
    pub entries: Vec<Entry>,
    /// Other mdta boxes in the same file, to be folded into `base` and then
    /// removed.
    ///
    /// A file that has been written in place by the exiftool path carries one
    /// of these: the key went into the file's own box, the value into a second
    /// box exiftool made (§8). ffprobe then pairs the stray box's items
    /// against the first box's key table, so leaving it in place makes one
    /// field read as another's value -- which is how this was found. Absorbing
    /// it repairs the file instead of inheriting the collision.
    pub absorb: Vec<String>,
}

/// Read a file's mdta box. `Ok(None)` means "this writer does not handle this
/// file" -- a caller falls back rather than treating it as an error.
///
/// Only `moov` is read. Planning happens on every confirmation, and a file
/// here is routinely gigabytes; reading the whole of one to find a 25 KB box
/// would make the plan cost more than the write.
pub fn survey(path: &Path) -> Result<Option<Survey>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let len = f.seek(SeekFrom::End(0))?;
    let mut at = 0u64;
    while at + 8 <= len {
        f.seek(SeekFrom::Start(at))?;
        let mut hdr = [0u8; 16];
        let n = f.read(&mut hdr)?;
        if n < 8 {
            return Ok(None);
        }
        let (size, hlen) = match be32(&hdr[0..4]) {
            1 if n >= 16 => (u64::from_be_bytes(hdr[8..16].try_into().unwrap()), 16),
            0 => (len - at, 8),
            v => (v as u64, 8),
        };
        if size < hlen {
            return Ok(None);
        }
        match &hdr[4..8] {
            // Fragmented: the sample offsets live in boxes the fixup does not
            // cover, so the file is declined before anything is rewritten.
            b"moof" => return Ok(None),
            b"moov" => {
                let mut buf = vec![0u8; size.min(len - at) as usize];
                f.seek(SeekFrom::Start(at))?;
                f.read_exact(&mut buf)?;
                let (mut found, mut frag) = (Vec::new(), false);
                scan(&buf, 0, buf.len(), "", &mut found, &mut frag);
                return Ok(if frag { None } else { fold(found) });
            }
            _ => {}
        }
        at += size;
    }
    Ok(None)
}

/// The parse, split out so every test in this module runs on bytes it built
/// itself rather than on media.
#[cfg(test)]
pub fn survey_bytes(buf: &[u8]) -> Option<Survey> {
    let mut found = Vec::new();
    let mut fragmented = false;
    scan(buf, 0, buf.len(), "", &mut found, &mut fragmented);
    if fragmented {
        return None;
    }
    fold(found)
}

/// The file's own box wins; anything only a stray box has is appended to it,
/// because a key no field claims is still never dropped.
fn fold(mut boxes: Vec<Survey>) -> Option<Survey> {
    if boxes.is_empty() {
        return None;
    }
    let mut primary = boxes.remove(0);
    for other in boxes {
        for e in other.entries {
            if !primary.entries.iter().any(|p| p.name == e.name) {
                primary.entries.push(e);
            }
        }
        primary.absorb.push(other.base);
    }
    Some(primary)
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn scan(buf: &[u8], mut off: usize, end: usize, path: &str, out: &mut Vec<Survey>, frag: &mut bool) {
    while off + 8 <= end {
        let size = be32(&buf[off..off + 4]) as usize;
        let typ = &buf[off + 4..off + 8];
        // A 64-bit or to-end-of-file box is only ever `mdat` here, which this
        // writer never looks inside; stopping the walk would be wrong, but
        // sizing it correctly is enough.
        let size = match size {
            1 if off + 16 <= end => {
                u64::from_be_bytes(buf[off + 8..off + 16].try_into().unwrap()) as usize
            }
            0 => end - off,
            n => n,
        };
        if size < 8 || off + size > end {
            return;
        }
        // Fragmented files keep their sample offsets in `moof` boxes the
        // chunk-offset fixup does not cover, so the whole file is declined.
        if typ == b"moof" || typ == b"mvex" {
            *frag = true;
            return;
        }
        let here = format!("{path}/{}", String::from_utf8_lossy(typ));
        match typ {
            b"moov" | b"udta" => scan(buf, off + 8, off + size, &here, out, frag),
            b"meta" => {
                // QuickTime's `meta` is not a FullBox: Apple writes `hdlr`
                // straight after the header where ISO writes four version and
                // flags bytes first. Detect it rather than assuming either.
                let qt = buf.len() >= off + 16 && &buf[off + 12..off + 16] == b"hdlr";
                let child = off + if qt { 8 } else { 12 };
                // A file can carry several. The first is the file's own --
                // the one both readers already agree on -- and the rest are
                // folded into it.
                if let Some(s) = read_meta(buf, child, off + size, &here) {
                    out.push(s);
                }
            }
            _ => {}
        }
        off += size;
    }
}

/// Parse one `meta` box, if it is an mdta one carrying `keys` and `ilst`.
fn read_meta(buf: &[u8], mut off: usize, end: usize, path: &str) -> Option<Survey> {
    let (mut names, mut items, mut is_mdta) = (Vec::new(), Vec::new(), false);
    while off + 8 <= end {
        let size = be32(&buf[off..off + 4]) as usize;
        if size < 8 || off + size > end {
            return None;
        }
        match &buf[off + 4..off + 8] {
            b"hdlr" if off + 20 <= end => is_mdta = &buf[off + 16..off + 20] == b"mdta",
            b"keys" if off + 16 <= end => {
                let count = be32(&buf[off + 12..off + 16]) as usize;
                let mut o = off + 16;
                for _ in 0..count {
                    if o + 8 > off + size {
                        return None;
                    }
                    let ks = be32(&buf[o..o + 4]) as usize;
                    if ks < 8 || o + ks > off + size {
                        return None;
                    }
                    names.push(String::from_utf8_lossy(&buf[o + 8..o + ks]).into_owned());
                    o += ks;
                }
            }
            b"ilst" => {
                let mut o = off + 8;
                while o + 8 <= off + size {
                    let isz = be32(&buf[o..o + 4]) as usize;
                    if isz < 8 || o + isz > off + size {
                        return None;
                    }
                    items.push((be32(&buf[o + 4..o + 8]), buf[o + 8..o + isz].to_vec()));
                    o += isz;
                }
            }
            _ => {}
        }
        off += size;
    }
    if !is_mdta || names.is_empty() {
        return None;
    }
    Some(Survey {
        base: path.trim_start_matches('/').to_string(),
        entries: pair(&names, &items)?,
        absorb: Vec::new(),
    })
}

/// Pair each item with the key it indexes.
///
/// A key with no item is dropped: that is exactly the debris exiftool leaves
/// (§8), it holds no value, and rebuilding the box is the only chance to
/// clear it. An item pointing at a key that does not exist is the opposite
/// -- data whose name is gone -- so the file is declined rather than written
/// with the value discarded.
fn pair(names: &[String], items: &[(u32, Vec<u8>)]) -> Option<Vec<Entry>> {
    let mut out = Vec::with_capacity(items.len());
    for (idx, payload) in items {
        let name = names.get(idx.checked_sub(1)? as usize)?;
        out.push(Entry { name: name.clone(), payload: payload.clone() });
    }
    Some(out)
}

/// Apply the plan's writes. An empty value deletes, which is how junk keys are
/// cleared; everything the plan does not mention is carried through untouched
/// (the rule MP4Box breaks, §7).
pub fn apply(entries: &[Entry], writes: &[(String, String)]) -> Vec<Entry> {
    let mut out = entries.to_vec();
    for (key, value) in writes {
        let at = out.iter().position(|e| &e.name == key);
        match (at, value.is_empty()) {
            (Some(i), true) => {
                out.remove(i);
            }
            (Some(i), false) => out[i] = Entry::text(key, value),
            (None, true) => {}
            (None, false) => out.push(Entry::text(key, value)),
        }
    }
    out
}

/// `keys`: version and flags, a count, then one `mdta`-namespaced name each.
pub fn build_keys(entries: &[Entry]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        p.extend_from_slice(&((8 + e.name.len()) as u32).to_be_bytes());
        p.extend_from_slice(b"mdta");
        p.extend_from_slice(e.name.as_bytes());
    }
    boxed(b"keys", &p)
}

/// `ilst`: one item per entry, named by its 1-based index into `keys`. The
/// indices are rebuilt from the order rather than carried over, so a delete
/// cannot leave the two boxes disagreeing.
pub fn build_ilst(entries: &[Entry]) -> Vec<u8> {
    let mut p = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        p.extend_from_slice(&((8 + e.payload.len()) as u32).to_be_bytes());
        p.extend_from_slice(&((i + 1) as u32).to_be_bytes());
        p.extend_from_slice(&e.payload);
    }
    boxed(b"ilst", &p)
}

fn boxed(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + payload.len());
    b.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    b.extend_from_slice(typ);
    b.extend_from_slice(payload);
    b
}

/// Rewrite `src` into `dst` with `writes` applied. The source is only read.
///
/// `faststart` moves `moov` ahead of `mdat`; the chunk offsets that shift are
/// remapped by the crate, and an offset that cannot be represented is a
/// failure rather than a silently wrong file.
pub fn rewrite(
    src: &Path,
    dst: &Path,
    survey: &Survey,
    writes: &[(String, String)],
    faststart: bool,
) -> Result<()> {
    let entries = apply(&survey.entries, writes);
    let mut ed = Editor::new();
    ed.add_command(Command::Replace {
        path: format!("{}/keys", survey.base),
        bytes: build_keys(&entries),
    });
    ed.add_command(Command::Replace {
        path: format!("{}/ilst", survey.base),
        bytes: build_ilst(&entries),
    });
    for path in &survey.absorb {
        ed.add_command(Command::Remove { path: path.clone() });
    }
    if faststart {
        ed.add_command(Command::Faststart);
    }
    let stats = ed
        .process_file(src, dst)
        .with_context(|| format!("rewriting {}", src.display()))?;
    if stats.chunk_offsets_unmapped > 0 {
        bail!(
            "{} chunk offsets could not be remapped; refusing the result",
            stats.chunk_offsets_unmapped
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_box(qt: bool, names: &[&str], items: &[(u32, &str)]) -> Vec<u8> {
        let entries: Vec<Entry> = names.iter().map(|n| Entry::text(n, "")).collect();
        let keys = build_keys(&entries);
        let mut ilst = Vec::new();
        for (idx, v) in items {
            let e = Entry::text("x", v);
            ilst.extend_from_slice(&((8 + e.payload.len()) as u32).to_be_bytes());
            ilst.extend_from_slice(&idx.to_be_bytes());
            ilst.extend_from_slice(&e.payload);
        }
        let ilst = boxed(b"ilst", &ilst);

        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&0u32.to_be_bytes());
        hdlr.extend_from_slice(&0u32.to_be_bytes());
        hdlr.extend_from_slice(b"mdta");
        hdlr.extend_from_slice(&[0u8; 12]);
        let hdlr = boxed(b"hdlr", &hdlr);

        let mut p = Vec::new();
        if !qt {
            p.extend_from_slice(&0u32.to_be_bytes()); // ISO version + flags
        }
        p.extend_from_slice(&hdlr);
        p.extend_from_slice(&keys);
        p.extend_from_slice(&ilst);
        boxed(b"meta", &p)
    }

    fn file(meta: &[u8], nested_in_udta: bool) -> Vec<u8> {
        let inner = if nested_in_udta { boxed(b"udta", meta) } else { meta.to_vec() };
        let mut moov = Vec::new();
        moov.extend_from_slice(&inner);
        let mut f = boxed(b"ftyp", b"qt  ");
        f.extend_from_slice(&boxed(b"mdat", b"not looked at"));
        f.extend_from_slice(&boxed(b"moov", &moov));
        f
    }

    #[test]
    fn finds_the_apple_layout_and_the_ffmpeg_one() {
        let apple = file(&meta_box(true, &["title"], &[(1, "a")]), false);
        let ffmpeg = file(&meta_box(false, &["title"], &[(1, "a")]), true);
        assert_eq!(survey_bytes(&apple).unwrap().base, "moov/meta");
        assert_eq!(survey_bytes(&ffmpeg).unwrap().base, "moov/udta/meta");
    }

    /// The QuickTime `meta` box has no version/flags word. Reading it as if it
    /// did lands four bytes inside `hdlr` and finds nothing at all.
    #[test]
    fn a_quicktime_meta_is_not_a_fullbox() {
        let s = survey_bytes(&file(&meta_box(true, &["title"], &[(1, "hello")]), false)).unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].as_text(), Some("hello"));
    }

    /// exiftool's debris: a key with no item behind it (CONTAINER §8). It
    /// carries no value, and rebuilding the box is the one chance to drop it.
    #[test]
    fn a_key_with_no_item_is_dropped() {
        let s = survey_bytes(&file(&meta_box(false, &["title", "orphan"], &[(1, "a")]), true))
            .unwrap();
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].name, "title");
    }

    /// The opposite case is data whose name is gone, so the file is declined
    /// rather than rewritten with the value quietly discarded.
    #[test]
    fn an_item_with_no_key_declines_the_file() {
        let f = file(&meta_box(false, &["title"], &[(1, "a"), (9, "lost")]), true);
        assert!(survey_bytes(&f).is_none());
    }

    #[test]
    fn a_fragmented_file_is_declined() {
        let mut f = file(&meta_box(false, &["title"], &[(1, "a")]), true);
        f.extend_from_slice(&boxed(b"moof", b""));
        assert!(survey_bytes(&f).is_none());
    }

    #[test]
    fn writes_update_add_and_delete() {
        let start = vec![Entry::text("title", "old"), Entry::text("junk", "x")];
        let out = apply(
            &start,
            &[
                ("title".into(), "new".into()),
                ("junk".into(), String::new()),
                ("origin".into(), "added".into()),
            ],
        );
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["title", "origin"]);
        assert_eq!(out[0].as_text(), Some("new"));
        assert_eq!(out[1].as_text(), Some("added"));
    }

    /// The whole point: every item's index resolves to the key of the same
    /// name, with nothing dangling either way (CONTAINER §8).
    #[test]
    fn the_two_boxes_always_agree() {
        let entries = apply(
            &[Entry::text("a", "1"), Entry::text("b", "2"), Entry::text("c", "3")],
            &[("b".into(), String::new()), ("d".into(), "4".into())],
        );
        let keys = build_keys(&entries);
        let ilst = build_ilst(&entries);
        // Rebuild a file around the two boxes and read it back the way ffprobe
        // pairs them.
        let mut p = vec![0, 0, 0, 0];
        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0u8; 8]);
        hdlr.extend_from_slice(b"mdta");
        hdlr.extend_from_slice(&[0u8; 12]);
        p.extend_from_slice(&boxed(b"hdlr", &hdlr));
        p.extend_from_slice(&keys);
        p.extend_from_slice(&ilst);
        let back = survey_bytes(&file(&boxed(b"meta", &p), true)).unwrap();
        let pairs: Vec<(&str, Option<&str>)> =
            back.entries.iter().map(|e| (e.name.as_str(), e.as_text())).collect();
        assert_eq!(pairs, [("a", Some("1")), ("c", Some("3")), ("d", Some("4"))]);
    }

    /// A file written in place by the exiftool path carries a second mdta box
    /// holding the value, with the key left behind in the first (CONTAINER
    /// §8). Both are read, the stray one is folded in, and its box is marked
    /// for removal -- otherwise ffprobe pairs its items against the first
    /// box's key table and one field reads as another's value.
    #[test]
    fn a_stray_second_box_is_absorbed() {
        let own = meta_box(false, &["title", "origin"], &[(1, "kept")]);
        let stray = meta_box(true, &["com.apple.quicktime.origin"], &[(1, "stray")]);
        let mut moov = boxed(b"udta", &own);
        moov.extend_from_slice(&stray);
        let mut f = boxed(b"ftyp", b"qt  ");
        f.extend_from_slice(&boxed(b"moov", &moov));

        let s = survey_bytes(&f).unwrap();
        assert_eq!(s.base, "moov/udta/meta");
        assert_eq!(s.absorb, ["moov/meta"]);
        let pairs: Vec<(&str, Option<&str>)> =
            s.entries.iter().map(|e| (e.name.as_str(), e.as_text())).collect();
        assert_eq!(
            pairs,
            [("title", Some("kept")), ("com.apple.quicktime.origin", Some("stray"))],
            "the file's own box wins, and the stray value is kept rather than dropped"
        );
    }

    /// A file's own keys include floats and integers. A rewrite that touches
    /// one string must carry the rest through byte for byte.
    #[test]
    fn a_typed_payload_is_carried_through_untouched() {
        let typed = Entry {
            name: "com.apple.quicktime.camera.focal_length.35mm_equivalent".into(),
            // A `data` box of well-known type 21 (signed int), not text.
            payload: vec![0, 0, 0, 18, b'd', b'a', b't', b'a', 0, 0, 0, 21, 0, 0, 0, 0, 0, 26],
        };
        let out = apply(&[typed.clone()], &[("title".into(), "new".into())]);
        assert_eq!(out[0], typed);
        assert_eq!(out[0].as_text(), None, "a typed payload must not read as text");
    }

    /// The one test here that needs a real file, because the layouts that
    /// matter cannot be synthesised: `TAGFORM_FIXTURE=path cargo test --
    /// --ignored`. It writes to a copy, never to the file named.
    ///
    /// This is the seed of the fixture suite DESIGN §14 asks for and §9.5
    /// gates the removal of the old writers on.
    #[test]
    #[ignore = "needs real media; set TAGFORM_FIXTURE"]
    fn a_real_file_keeps_its_tracks_and_gains_a_key() {
        let src = std::path::PathBuf::from(std::env::var("TAGFORM_FIXTURE").unwrap());
        let dst = std::env::temp_dir().join("tagform-native-fixture.mov");
        std::fs::copy(&src, &dst).unwrap();

        let before = crate::tags::write::probe_streams(&dst);
        let found = survey(&dst).unwrap().expect("a layout the writer handles");
        let had = found.entries.len();

        let out = std::env::temp_dir().join("tagform-native-fixture.out.mov");
        rewrite(&dst, &out, &found, &[("origin".into(), "fixture".into())], false).unwrap();

        assert_eq!(crate::tags::write::probe_streams(&out), before, "a track changed");
        let after = survey(&out).unwrap().unwrap();
        assert_eq!(after.entries.len(), had + 1);
        // The proof that §3.2 is gone: the reader that could not see an added
        // key sees this one.
        let tags = crate::tags::probe::probe(&out).unwrap();
        assert_eq!(
            tags.atoms.get("origin").map(|v| format!("{v:?}")),
            Some("Text(\"fixture\")".to_string())
        );
    }
}
