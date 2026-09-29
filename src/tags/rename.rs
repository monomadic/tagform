//! Renaming a file from its own tags, by handing it to `rename-video`.
//!
//! Filename sync as designed (DESIGN §9.4) is unbuilt, and the grammar it
//! describes already exists outside this repo: `rename-video` composes exactly
//! those two names from the same atoms and XMP tags `tagform` writes, and
//! chooses between them on Category the same way. Shelling out to it keeps one
//! grammar in one place — a second implementation here would be another parser
//! in a library that already has several, and the first one able to disagree
//! with the rest.
//!
//! Matroska is the exception to "by handing it to `rename-video`": the script
//! reads atoms and XMP and refuses anything but MP4/MOV, so a Matroska file is
//! named here, from the tags `tags/mkv.rs` reads, by the grammar in
//! `model/namer.rs` -- the script's own, as a template string a config file
//! can override once there is one. The move, and the checks around it, are
//! shared.
//!
//! Nothing in this module writes tags. The tool re-probes the file
//! itself, so the name it builds is the name the file *on disk* has earned;
//! staged edits are not in it, which is why the caller refuses to run while any
//! are outstanding.

use anyhow::{bail, Context, Result};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::namer::{self, Category, Fields, Spec, Templates};
use crate::model::schema::field_by_id;
use crate::tags::mkv;
use crate::tags::probe::{self, FileTags};
use crate::model::value::Value;

/// Looked up on PATH, like every other external tool here. Optional: a missing
/// `rename-video` costs you `r` and nothing else.
const TOOL: &str = "rename-video";

/// Bytes a filename may hold on APFS, HFS+, ext4 and NTFS alike. The tool
/// composes a name out of every tag on the file, and a long tag list walks
/// straight past this; `mv` then fails with a message that puts the reason
/// after two 300-byte paths, where a one-line status cannot reach it.
pub(crate) const NAME_MAX: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Renamed, and now lives here.
    Renamed(PathBuf),
    /// The name already says what the tags say. Nothing was touched.
    Unchanged,
    /// Another file already holds the name these tags ask for. `rename-video`
    /// declines rather than disambiguating with a counter, and so do we: two
    /// files whose tags produce one name is a tagging problem.
    Taken(PathBuf),
}

/// The tool as `Command` should be given it: the bare name when PATH has it,
/// otherwise the copy installed beside this binary, if there is one.
///
/// `tagform` gets started from places whose PATH is not the login shell's -- a
/// file manager's opener, a launcher, a terminal that was open before the tool
/// was installed -- and by an absolute path, so it runs while its sibling in
/// the same `bin` does not. The two are installed together; look there before
/// giving up.
fn tool() -> PathBuf {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(TOOL).is_file()));
    if !on_path {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|e| Some(e.parent()?.join(TOOL)));
        if let Some(beside) = beside.filter(|b| b.is_file()) {
            return beside;
        }
    }
    PathBuf::from(TOOL)
}

/// Why the tool did not start. A missing binary is the one failure here with
/// a fix the user can act on, and "No such file or directory" beside a video's
/// name reads as though the *video* were missing.
fn spawn_error(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!("{TOOL} is not on PATH")
    } else {
        anyhow::Error::new(e).context(format!("running {TOOL}"))
    }
}

/// Where the file wants to live, asked without letting anything move.
///
/// `--print-target` is the tool's own dry-run answer, which is what makes the
/// collision check below a filesystem question rather than an exercise in
/// parsing decorated output.
pub fn target(path: &Path) -> Result<PathBuf> {
    if mkv::is_matroska(path) {
        return matroska_target(path, &Templates::default());
    }
    let out = Command::new(tool())
        .arg("--print-target")
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    if !out.status.success() {
        bail!("{}", say(&out.stderr, &out.stdout));
    }
    // A file the tool declines — no category, an extension it does not take —
    // gets a warning on stdout and a zero exit, so a target is only a target
    // when it is an absolute path. Its refusals say more than any sentence
    // written here could, so they are passed through as the error.
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !line.starts_with('/') {
        bail!("{}", say(&out.stderr, &out.stdout));
    }
    let target = PathBuf::from(line);
    let len = target.file_name().map_or(0, |n| n.as_encoded_bytes().len());
    if len > NAME_MAX {
        bail!("name would be {len} bytes and the filesystem allows {NAME_MAX} -- fewer tags would fit");
    }
    Ok(target)
}

/// Where a Matroska file wants to live: its own directory, and the name its
/// tags earn. Refused, with the script's wording, when it carries too little
/// to be named -- the fix is to tag it, not to name it more cleverly.
fn matroska_target(path: &Path, templates: &Templates) -> Result<PathBuf> {
    let tags = probe::probe(path)?;
    let fields = fields_of(&tags)?;
    if let Some(why) = fields.missing() {
        bail!("Cannot name {}: {why}. Tag it first.", name_of(path));
    }
    let current = name_of(path);
    let ext = path.extension().map(|e| e.to_string_lossy()).unwrap_or_default();
    let Some(file) = namer::name(&fields, templates, &current, &ext) else {
        bail!("No actors, channel or title to name it after: {current}");
    };
    Ok(path.with_file_name(file))
}

/// The naming fields a file's tags give, and its probed spec. The precedence
/// is `FileTags::lookup`'s -- XMP, then atoms -- which for Matroska is the
/// one spelling its tags have.
fn fields_of(tags: &FileTags) -> Result<Fields> {
    let get = |id: &str| field_by_id(id).and_then(|f| tags.lookup(f));
    let text = |id: &str| -> String {
        match get(id) {
            Some(Value::Text(s)) => s.trim().to_string(),
            Some(Value::List(v)) => v.join(", "),
            None => String::new(),
        }
    };
    let list = |id: &str| -> Vec<String> {
        let items = match get(id) {
            Some(Value::List(v)) => v,
            Some(Value::Text(s)) => s.split(',').map(str::to_string).collect(),
            None => Vec::new(),
        };
        items
            .into_iter()
            .map(|i| i.trim().to_string())
            .filter(|i| !i.is_empty())
            .collect()
    };
    let atom = |k: &str| match tags.atoms.get(k) {
        Some(Value::Text(s)) => s.trim().to_string(),
        _ => String::new(),
    };

    let category = Some(text("category"))
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| text("genre"));

    // The page the file was fetched from, in the tags yt-dlp writes for it; a
    // comment counts only when it looks like a URL, the field being prose.
    let has_source = ["original_url", "purl", "source_url"]
        .iter()
        .any(|k| !atom(k).is_empty())
        || atom("comment").contains("://");

    // A clip's number is the Track; a remux may have made it "N/M".
    let clip = text("variant")
        .eq_ignore_ascii_case("clip")
        .then(|| text("track"))
        .and_then(|t| t.split('/').next().and_then(|n| n.trim().parse::<u32>().ok()))
        .filter(|&n| n > 0);

    let city = text("location");
    let path = &tags.path;
    Ok(Fields {
        category: Category::from_tag(&category),
        actors: list("actors"),
        channel: text("channel"),
        title: text("title"),
        tags: list("tags"),
        rating: text("rating"),
        clip,
        orientation: text("orientation"),
        // The city names the clip; a venue stands in only when none was written.
        location: if city.is_empty() { text("location_place") } else { city },
        date: stamp(&text("date")).or_else(|| birth_stamp(path)).unwrap_or_default(),
        has_source,
        spec: spec_of(path, &atom("com.apple.quicktime.model"))?,
    })
}

/// `2025-02-02T12:23:44.000000Z` -> `2025-02-02--12-23-44`. Any date and time
/// with the digits in that order will do, `:` or `-` between the date's; a
/// year of zero is a muxer's way of saying nothing.
fn stamp(raw: &str) -> Option<String> {
    let d: Vec<u32> = raw
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .take(6)
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let [y, mo, day, h, mi, s] = <[u32; 6]>::try_from(d).ok()?;
    (y != 0).then(|| format!("{y:04}-{mo:02}-{day:02}--{h:02}-{mi:02}-{s:02}"))
}

/// When the file first existed, for a file that says nothing of when it was
/// shot.
fn birth_stamp(path: &Path) -> Option<String> {
    let t = std::fs::metadata(path).ok()?.created().ok()?;
    let t: chrono::DateTime<chrono::Local> = t.into();
    Some(t.format("%Y-%m-%d--%H-%M-%S").to_string())
}

/// The spec block, probed fresh: what the file *is*, whatever its tags say.
fn spec_of(path: &Path, model: &str) -> Result<Spec> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-show_format", "-of", "json", "--"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("running ffprobe (is it installed?)")?;
    if !out.status.success() {
        bail!("Could not read video info: {}", name_of(path));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing ffprobe json")?;
    let video = v["streams"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1))
        .with_context(|| format!("Could not read video info: {}", name_of(path)))?;
    let num = |x: &serde_json::Value| -> Option<f64> {
        x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    };
    let (mut w, mut h) = (num(&video["width"]).unwrap_or(0.0), num(&video["height"]).unwrap_or(0.0));
    if w == 0.0 || h == 0.0 {
        bail!("Could not read video info: {}", name_of(path));
    }
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| num(&d["rotation"])))
        .or_else(|| num(&video["tags"]["rotate"]))
        .unwrap_or(0.0)
        .abs();
    if rotation == 90.0 || rotation == 270.0 {
        std::mem::swap(&mut w, &mut h);
    }
    let short = w.min(h) as u32;
    let resolution = match short {
        0..=480 => "480p".into(),
        481..=720 => "720p".into(),
        721..=1080 => "1080p".into(),
        1081..=1440 => "1440p".into(),
        1441..=2160 => "2160p".into(),
        n => format!("{n}p"),
    };
    let fps = video["r_frame_rate"]
        .as_str()
        .and_then(|r| r.split_once('/'))
        .and_then(|(n, d)| Some((n.parse::<u64>().ok()?, d.parse::<u64>().ok()?)))
        .map(|(n, d)| if d > 0 { (n + d / 2) / d } else { 0 })
        .unwrap_or(0);
    let secs = num(&v["format"]["duration"]).unwrap_or(0.0).round() as u64;
    let duration = if secs >= 3600 {
        format!("{}hr", secs / 3600)
    } else if secs >= 60 {
        format!("{}min", secs / 60)
    } else {
        format!("{secs}sec")
    };
    let mbps = num(&video["bit_rate"])
        .map(|b| format!("{}mbps", ((b + 500_000.0) / 1_000_000.0) as u64))
        .unwrap_or_default();
    let codec = match video["codec_name"].as_str().unwrap_or("").to_lowercase().as_str() {
        "hevc" | "h265" => "h265",
        "h264" | "avc1" => "h264",
        "prores" => "PRO",
        "vp9" => "VP9",
        "av1" => "AV1",
        _ => "",
    };
    Ok(Spec {
        resolution,
        fps: format!("{fps}fps"),
        duration,
        mbps,
        codec: codec.into(),
        // One word: "iPhone 15 Pro" -> "iPhone15Pro".
        device: model.split_whitespace().collect(),
        shape: match w.partial_cmp(&h) {
            Some(std::cmp::Ordering::Greater) => "H",
            Some(std::cmp::Ordering::Less) => "V",
            _ => "S",
        }
        .into(),
    })
}

/// Move `from` to `to`, and never onto a name something else holds.
///
/// A hard link is the no-clobber primitive: it fails if the name exists, in
/// one step, where check-then-rename has a window a download or a second run
/// can land in. A volume with no links falls back to a rename, which is only
/// as safe as the check the caller already made. A case-only rename on a
/// case-insensitive volume is the same file under a new spelling, so it
/// goes through a name of its own.
fn move_noclobber(from: &Path, to: &Path) -> Result<()> {
    let refused = || {
        anyhow::anyhow!(
            "Refused: {} appeared while renaming {}",
            name_of(to),
            name_of(from)
        )
    };
    if entry_exists(to) || ident(to).is_some() {
        if ident(from) != ident(to) {
            return Err(refused());
        }
        let stage = from.with_file_name(format!(".tagform-rename.{}", std::process::id()));
        if entry_exists(&stage) {
            return Err(refused());
        }
        std::fs::rename(from, &stage)?;
        if let Err(e) = std::fs::hard_link(&stage, to).or_else(|_| std::fs::rename(&stage, to)) {
            let _ = std::fs::rename(&stage, from);
            return Err(e.into());
        }
        let _ = std::fs::remove_file(&stage);
        return Ok(());
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from).context("removing the old name"),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(refused()),
        Err(_) => std::fs::rename(from, to).map_err(Into::into),
    }
}

/// Rename `path` from its tags. The returned path is where the file now is.
///
/// Three checks stand between a target and a lost file, and each covers what
/// the others cannot. `same_entry` recognises the file that is already
/// correctly named -- the tool answers `--print-target` in absolute paths and
/// `tagform` is routinely handed relative ones, so the two spellings of one
/// file are not comparable as strings. `entry_exists` refuses a target another
/// file holds. And the identity check afterwards proves that what now sits at
/// the target is the file that was asked to move, not a stranger that arrived
/// between the check and the rename.
pub fn run(path: &Path) -> Result<Outcome> {
    let target = target(path)?;
    if same_entry(path, &target) {
        return Ok(Outcome::Unchanged);
    }
    if taken(path, &target) {
        return Ok(Outcome::Taken(target));
    }
    let before = ident(path);
    let out = if mkv::is_matroska(path) {
        move_noclobber(path, &target)?;
        None
    } else {
        let out = Command::new(tool())
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .map_err(spawn_error)?;
        if !out.status.success() {
            bail!("{}", say(&out.stderr, &out.stdout));
        }
        Some(out)
    };
    // A zero exit is not proof of a rename: every refusal the tool has left
    // after the target check is a warning printed with a zero status. Ask the
    // directory instead.
    if !entry_exists(&target) {
        match out {
            Some(o) => bail!("{}", say(&o.stderr, &o.stdout)),
            None => bail!("{} was not renamed", name_of(path)),
        }
    }
    // And an entry at the target is not proof either. The tool declines to
    // overwrite, so a target that holds a *different* file than the one handed
    // over means the rename did not happen and something else got there first
    // -- report that rather than tell the caller its file moved somewhere it
    // did not.
    if let (Some(before), Some(after)) = (before, ident(&target)) {
        if before != after {
            bail!(
                "{} is another file now; {} was left where it was",
                name_of(&target),
                name_of(path)
            );
        }
    }
    Ok(Outcome::Renamed(target))
}

/// The file another file would be overwritten by, if `r` ran on `path` now:
/// the target, when something other than `path` already holds it. Asked
/// ahead of any `r`, so a collision is on screen before the rename that
/// would meet it -- the rename refuses it either way.
pub fn conflict(path: &Path) -> Result<Option<PathBuf>> {
    let target = target(path)?;
    Ok((!same_entry(path, &target) && taken(path, &target)).then_some(target))
}

/// Something other than `path` already answers to `target`. The exact entry,
/// or -- on a case-insensitive volume -- a different file that the name
/// resolves to anyway: `Clip.mov` over an unrelated `clip.mov` is still an
/// overwrite, though no entry is spelled `Clip.mov`. A case-only rename of
/// the file itself resolves to its own inode and is not taken.
fn taken(path: &Path, target: &Path) -> bool {
    entry_exists(target) || ident(target).is_some_and(|t| Some(t) != ident(path))
}

/// The two paths name one directory entry: the same last component, and the
/// same file underneath it.
///
/// Both halves are load-bearing. Without the identity check a file moved out
/// from under us reads as unchanged; without the name check a hard link -- two
/// names, one inode, and the second one a file this rename must not take --
/// reads as the same entry and gets skipped instead of refused.
fn same_entry(a: &Path, b: &Path) -> bool {
    a.file_name() == b.file_name() && ident(a).is_some() && ident(a) == ident(b)
}

/// Which file a path names, if it names one. `symlink_metadata`, so a symlink
/// is identified as itself: it is the entry that moves, and the file it points
/// at is not this tool's business.
fn ident(p: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(p)
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .unwrap_or(p.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Does the directory hold an entry with exactly this name?
///
/// `Path::exists` cannot answer that on a case-insensitive volume, where
/// `clip.mov` and `Clip.mov` are one file — and a case-only rename is precisely
/// what `r` is for after a title has been recapitalised. Read as a collision it
/// would refuse the rename that was asked for; read as proof of success it
/// would report one that never happened.
fn entry_exists(p: &Path) -> bool {
    let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else {
        return false;
    };
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| e.file_name() == name))
        .unwrap_or(false)
}

/// The tool's own first word on the matter, with its status glyph trimmed off.
/// Failures go to stderr and refusals to stdout, so both are looked at, in that
/// order.
fn say(err: &[u8], out: &[u8]) -> String {
    let err = String::from_utf8_lossy(err).into_owned();
    let out = String::from_utf8_lossy(out).into_owned();
    err.lines()
        .chain(out.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| reason_first(l.trim_start_matches(|c: char| !c.is_alphanumeric())))
        .unwrap_or_else(|| format!("{TOOL} said nothing and did nothing"))
}

/// `mv: rename <src> to <dst>: File name too long` -- the part that says what
/// went wrong sits after both paths, and the paths are the long part. Lift it
/// to the front so it survives a status line that shows the first sixty
/// columns and no more.
fn reason_first(line: &str) -> String {
    let Some(rest) = line.strip_prefix("mv: ") else {
        return line.to_string();
    };
    match rest.rsplit_once(": ") {
        Some((_, reason)) if !reason.is_empty() => format!("mv: {reason}"),
        _ => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_prefers_stderr_and_drops_the_glyph() {
        let e = b"\xe2\x9a\xa0 No category tag: clip.mov\n  Tag it first.\n";
        assert_eq!(say(e, b"ignored\n"), "No category tag: clip.mov");
        assert_eq!(
            say(b"", b"\xe2\x9c\x93 Exists: clip.mov\n"),
            "Exists: clip.mov"
        );
        assert!(say(b"", b"").contains(TOOL));
    }

    #[test]
    fn say_puts_the_mv_reason_before_the_paths() {
        let e = b"mv: rename /a/very long (name).mp4 to /a/longer (name): File name too long\n";
        assert_eq!(say(e, b""), "mv: File name too long");
        assert_eq!(say(b"mv: no reason here\n", b""), "mv: no reason here");
    }

    #[test]
    fn same_entry_sees_through_a_relative_path_but_not_through_a_link() {
        let dir = std::env::temp_dir().join("tagform-rename-ident");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mov");
        std::fs::write(&file, b"x").unwrap();

        // The case `r` hits constantly: `tagform clip.mov` from the file's own
        // directory, against the absolute path `--print-target` answers with.
        let relative = dir.join(".").join("clip.mov");
        assert!(same_entry(&relative, &file));

        // One inode, two names -- the second is a file of its own, and a
        // rename onto it would take it.
        let link = dir.join("other.mov");
        std::fs::hard_link(&file, &link).unwrap();
        assert!(!same_entry(&file, &link));

        assert!(!same_entry(&file, &dir.join("absent.mov")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A target is taken by any other file, and never by the file itself.
    #[test]
    fn taken_is_any_file_but_this_one() {
        let dir = std::env::temp_dir().join("tagform-rename-taken");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mov");
        let other = dir.join("other.mov");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(&other, b"y").unwrap();
        assert!(taken(&file, &other));
        assert!(!taken(&file, &dir.join("free.mov")));
        // A case-only rename: on a case-insensitive volume the new spelling
        // resolves to this same file, which is not a collision.
        assert!(!taken(&file, &dir.join("Clip.mov")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_exists_is_exact_about_case() {
        let dir = std::env::temp_dir().join("tagform-rename-case");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Clip.mov"), b"x").unwrap();
        assert!(entry_exists(&dir.join("Clip.mov")));
        // The point of the whole helper: on a case-insensitive volume this path
        // reports `exists()` as true, and it is not an entry.
        assert!(!entry_exists(&dir.join("clip.mov")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Matroska file is named here, from its own tags, and moved without
    /// `rename-video` -- which would refuse it. Needs ffmpeg.
    #[test]
    fn a_matroska_file_is_named_from_its_tags_and_moved() {
        let dir = std::env::temp_dir().join("tagform-rename-mkv");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dl.mkv");
        let ok = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=25:duration=1"])
            .args(["-metadata", "title=Some Title", "-metadata", "category=Adult"])
            .args(["-metadata", "actors=Alice, Bob", "-metadata", "channel=Studio"])
            .args(["-metadata", "keywords=one,two", "-metadata", "rating=4"])
            .arg(&file)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg could not make the fixture");

        let want = dir.join("Alice, Bob (Studio) - Some Title #one #two ★★★★☆ [480p 25fps h264 1sec H].mkv");
        assert_eq!(target(&file).unwrap(), want);
        assert_eq!(run(&file).unwrap(), Outcome::Renamed(want.clone()));
        assert!(want.is_file() && !file.exists());
        // Named already: nothing to do.
        assert_eq!(run(&want).unwrap(), Outcome::Unchanged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stamp_is_read_from_any_iso_date() {
        assert_eq!(stamp("2025-02-02T12:23:44.000000Z").as_deref(), Some("2025-02-02--12-23-44"));
        assert_eq!(stamp("2025:02:02 12:23:44").as_deref(), Some("2025-02-02--12-23-44"));
        assert_eq!(stamp("0000-00-00 00:00:00"), None);
        assert_eq!(stamp("2025-02-02"), None);
    }
}
