//! Seeding a file's fields from the page it was downloaded from (DESIGN §5.5).
//!
//! `d` over the URL field asks yt-dlp for the page's metadata -- the same
//! `info.json` a download would have produced, without the download -- and
//! stages what it finds onto the other fields. The mapping below is the one
//! `~/.config/yt-dlp/config` applies with `--parse-metadata` when it embeds
//! tags at download time, so a file fetched here and a file downloaded there
//! end up carrying the same values under the same keys. That config is the
//! authority on the mapping; this module restates it, and says so beside each
//! line.
//!
//! Nothing here reads or writes a container. The result is a list of field
//! values, and the app stages them like typed edits: undone with `u`, shown in
//! the plan `w` confirms, and never written until then. A field the page does
//! not answer is simply not in the list, which is what leaves the existing
//! value alone.

use anyhow::{bail, Context, Result};
use serde_json::Value as Json;
use std::process::{Command, Stdio};

use crate::model::value::Value;

/// Looked up on PATH, like every other external tool here. Optional: a missing
/// `yt-dlp` costs you `d` and nothing else.
const TOOL: &str = "yt-dlp";

/// Ask the page for its metadata and translate it into field values.
///
/// `-J` is `--dump-single-json`: the extractor runs, nothing is downloaded, and
/// the whole info dict comes back on stdout. The user's config is deliberately
/// *not* ignored -- it carries the cookies and extractor settings some sites
/// need -- but its download archive is, since an archived id would otherwise
/// come back as "already recorded" instead of as metadata. `--no-playlist`
/// keeps a video URL that happens to carry a list parameter from expanding
/// into the list.
pub fn fetch(url: &str) -> Result<Vec<(&'static str, Value)>> {
    let out = Command::new(TOOL)
        .args(["-J", "--skip-download", "--no-download-archive", "--no-playlist", "--no-warnings", "--"])
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {TOOL}"))?;
    if !out.status.success() {
        bail!("{}", say(&out.stderr));
    }
    let info: Json = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("{TOOL} did not return JSON"))?;
    if info.get("_type").and_then(Json::as_str) == Some("playlist") {
        bail!("that URL is a playlist, not a video");
    }
    Ok(from_info(&info))
}

/// The field values an info dict yields, in form order. Pure, so it can be
/// tested on a dict that never came from a network.
///
/// Each line names the `--parse-metadata` rule in the yt-dlp config it
/// mirrors. A key the dict lacks, or holds empty, yields nothing -- the caller
/// leaves that field as it was.
pub fn from_info(info: &Json) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    // title: --embed-metadata's own mapping.
    if let Some(t) = text(info, &["title"]) {
        out.push(("title", Value::Text(t)));
    }
    // actors: %(cast,uploader)l -- the cast where the site names one, else
    // whoever posted it.
    if let Some(l) = list(info, "cast").or_else(|| text(info, &["uploader"]).map(|u| vec![u])) {
        out.push(("actors", Value::List(l)));
    }
    // channel: %(channel,uploader)s.
    if let Some(c) = text(info, &["channel", "uploader"]) {
        out.push(("channel", Value::Text(c)));
    }
    // description: --embed-metadata's own mapping.
    if let Some(d) = text(info, &["description"]) {
        out.push(("description", Value::Text(d)));
    }
    // keywords: %(tags)l.
    if let Some(t) = list(info, "tags") {
        out.push(("tags", Value::List(t)));
    }
    // date: %(upload_date>%Y-%m-%d)s -- ISO, not yt-dlp's raw YYYYMMDD, which
    // is the shape the Date control accepts without a warning.
    if let Some(d) = text(info, &["upload_date"]).and_then(|d| iso_date(&d)) {
        out.push(("date", Value::Text(d)));
    }
    out
}

/// The first of `keys` holding non-blank text.
fn text(info: &Json, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| info.get(k).and_then(Json::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

/// A non-empty array of non-blank strings. yt-dlp's list fields are null, an
/// empty array, or strings; anything else in the array is skipped.
fn list(info: &Json, key: &str) -> Option<Vec<String>> {
    let items: Vec<String> = info
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!items.is_empty()).then_some(items)
}

/// `20091025` → `2009-10-25`. Anything that is not eight digits is not a date
/// worth staging.
fn iso_date(raw: &str) -> Option<String> {
    let d = raw.trim();
    if d.len() != 8 || !d.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]))
}

/// yt-dlp's first complaint, with its `ERROR:` prefix trimmed off. It prints
/// warnings above errors, and with `--no-warnings` the first line is the one
/// that matters.
fn say(err: &[u8]) -> String {
    String::from_utf8_lossy(err)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches("ERROR:").trim().to_string())
        .unwrap_or_else(|| format!("{TOOL} failed and said nothing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn get<'a>(v: &'a [(&str, Value)], id: &str) -> Option<&'a Value> {
        v.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
    }

    #[test]
    fn maps_the_fields_the_config_embeds() {
        let info = json!({
            "title": "A Title",
            "cast": ["Alice", "Bob"],
            "uploader": "someone",
            "channel": "The Channel",
            "description": "prose",
            "tags": ["one", "two"],
            "upload_date": "20091025",
        });
        let v = from_info(&info);
        assert_eq!(get(&v, "title"), Some(&Value::text("A Title")));
        assert_eq!(get(&v, "actors"), Some(&Value::List(vec!["Alice".into(), "Bob".into()])));
        assert_eq!(get(&v, "channel"), Some(&Value::text("The Channel")));
        assert_eq!(get(&v, "description"), Some(&Value::text("prose")));
        assert_eq!(get(&v, "tags"), Some(&Value::List(vec!["one".into(), "two".into()])));
        assert_eq!(get(&v, "date"), Some(&Value::text("2009-10-25")));
    }

    /// The config's `%(cast,uploader)l` and `%(channel,uploader)s`: a site
    /// with no cast list and no channel still names who posted it.
    #[test]
    fn falls_back_to_the_uploader() {
        let info = json!({ "uploader": "someone", "cast": [], "channel": null });
        let v = from_info(&info);
        assert_eq!(get(&v, "actors"), Some(&Value::List(vec!["someone".into()])));
        assert_eq!(get(&v, "channel"), Some(&Value::text("someone")));
    }

    /// A field the page does not answer must be absent from the result, not
    /// present and empty -- an empty value would stage a clear.
    #[test]
    fn an_unanswered_field_is_not_in_the_result() {
        let info = json!({ "title": "  ", "tags": [], "description": null, "upload_date": "2009" });
        let v = from_info(&info);
        assert!(v.is_empty(), "{v:?}");
    }

    #[test]
    fn dates_become_iso_or_nothing() {
        assert_eq!(iso_date("20091025").as_deref(), Some("2009-10-25"));
        assert_eq!(iso_date("2009-10-25"), None);
        assert_eq!(iso_date("NA"), None);
    }

    #[test]
    fn say_trims_the_error_prefix() {
        assert_eq!(say(b"\nERROR: [youtube] x: Video unavailable\n"), "[youtube] x: Video unavailable");
        assert!(say(b"").contains(TOOL));
    }
}
