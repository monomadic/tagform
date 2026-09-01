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
//! Nothing in this module reads or writes tags. The tool re-probes the file
//! itself, so the name it builds is the name the file *on disk* has earned;
//! staged edits are not in it, which is why the caller refuses to run while any
//! are outstanding.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Looked up on PATH, like every other external tool here. Optional: a missing
/// `rename-video` costs you `r` and nothing else.
const TOOL: &str = "rename-video";

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

/// Where the file wants to live, asked without letting anything move.
///
/// `--print-target` is the tool's own dry-run answer, which is what makes the
/// collision check below a filesystem question rather than an exercise in
/// parsing decorated output.
pub fn target(path: &Path) -> Result<PathBuf> {
    let out = Command::new(TOOL)
        .arg("--print-target")
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {TOOL}"))?;
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
    Ok(PathBuf::from(line))
}

/// Rename `path` from its tags. The returned path is where the file now is.
pub fn run(path: &Path) -> Result<Outcome> {
    let target = target(path)?;
    if target == path {
        return Ok(Outcome::Unchanged);
    }
    if entry_exists(&target) {
        return Ok(Outcome::Taken(target));
    }
    let out = Command::new(TOOL)
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {TOOL}"))?;
    if !out.status.success() {
        bail!("{}", say(&out.stderr, &out.stdout));
    }
    // A zero exit is not proof of a rename: every refusal the tool has left
    // after the target check is a warning printed with a zero status. Ask the
    // directory instead.
    if !entry_exists(&target) {
        bail!("{}", say(&out.stderr, &out.stdout));
    }
    Ok(Outcome::Renamed(target))
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
        .map(|l| l.trim_start_matches(|c: char| !c.is_alphanumeric()).to_string())
        .unwrap_or_else(|| format!("{TOOL} said nothing and did nothing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_prefers_stderr_and_drops_the_glyph() {
        let e = b"\xe2\x9a\xa0 No category tag: clip.mov\n  Tag it first.\n";
        assert_eq!(say(e, b"ignored\n"), "No category tag: clip.mov");
        assert_eq!(say(b"", b"\xe2\x9c\x93 Exists: clip.mov\n"), "Exists: clip.mov");
        assert!(say(b"", b"").contains(TOOL));
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
}

