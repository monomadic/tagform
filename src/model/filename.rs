//! Reading fields back out of a filename (DESIGN §9.4, the parsing half).
//!
//! The names in this library were composed by `ytform` and `rename-footage`
//! from the same tags this program edits, so a file whose container was never
//! tagged -- or was tagged by a tool that dropped half the keys -- still
//! carries its title, people, channel, tags and stars in its name. `i` then
//! `f` reads them back. The shapes it understands, extension aside:
//!
//! ```text
//! Actor, Actor (Channel) - Title #tag #tag ★★★★☆
//! Actor, Actor (Channel) - Title #tag #tag [meta] ★★★★☆
//! Actor (Channel) - Title #tag #tag [meta]
//! Actor - Title #tag #tag [meta]
//! 2024-05-01--13-22-08 Actor - Title #tag #tag [meta]
//! Title #tag ★★★☆☆
//! ```
//!
//! The parts are pulled off in the order that makes each one unambiguous: a
//! `[...]` block is the probed spec `rename-footage` appends and is never a
//! field, so it goes first; stars and `#tags` are single tokens anywhere in the
//! stem; a leading timestamp is a fixed shape; and only then is what remains
//! split on the first ` - ` into the people (with their `(Channel)`) and the
//! title. A name with no ` - ` is all title, because guessing that a plain
//! word is an actor is the wrong kind of helpful.
//!
//! Pure: a stem in, field values out, in the same list shape `fetch.rs`
//! returns, so the app stages both through one path. A field the name does
//! not carry is not in the list; what the caller does about a field that is
//! already filled is the caller's rule (the import never overwrites).

use std::path::Path;

use crate::model::tag;
use crate::model::value::Value;

/// A filled star. `☆` is the empty slot beside it -- decoration in the name,
/// and never counted.
const STAR: char = '★';
const EMPTY_STAR: char = '☆';

/// The field values a path's filename yields, in form order.
pub fn parse_path(path: &Path) -> Vec<(&'static str, Value)> {
    let stem = path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    parse(&stem)
}

/// The field values a stem yields. The extension is the caller's to drop.
pub fn parse(stem: &str) -> Vec<(&'static str, Value)> {
    let mut rest = strip_meta(stem);

    // Stars: the count of ★ in the run. An outline-only run (☆☆☆☆☆) is a
    // rating of zero, which stages nothing on an unrated file and is left to
    // the caller to compare -- the name did say "no stars", after all.
    let mut rating: Option<u8> = None;
    if rest.chars().any(|c| c == STAR || c == EMPTY_STAR) {
        let n = rest.chars().filter(|&c| c == STAR).count().min(5) as u8;
        rating = Some(n);
        rest = rest.chars().filter(|&c| c != STAR && c != EMPTY_STAR).collect();
    }

    // Hashtags: any whitespace-delimited token opening with `#`. Repaired
    // through the tag grammar, so a `#tag_two` in an old name lands as the
    // `tag-two` the form would have written.
    let mut tags: Vec<String> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for tok in rest.split_whitespace() {
        if tok.len() > 1 && tok.starts_with('#') {
            let t = tag::repair(tok);
            if !t.is_empty() && !tags.iter().any(|x| x.eq_ignore_ascii_case(&t)) {
                tags.push(t);
            }
        } else {
            kept.push(tok);
        }
    }
    let rest = kept.join(" ");

    // A leading timestamp, in the shape rename-footage writes.
    let (date, rest) = match take_date(&rest) {
        Some((d, r)) => (Some(d), r),
        None => (None, rest.as_str()),
    };

    // People and channel before the first ` - `, title after it.
    let (people, title) = match rest.split_once(" - ") {
        Some((l, r)) => (Some(l.trim()), r.trim()),
        None => (None, rest.trim()),
    };
    let (actors, channel) = match people {
        Some(p) if !p.is_empty() => split_people(p),
        _ => (Vec::new(), None),
    };

    let mut out = Vec::new();
    if let Some(d) = date {
        out.push(("date", Value::Text(d)));
    }
    if !title.is_empty() {
        out.push(("title", Value::Text(title.to_string())));
    }
    if !actors.is_empty() {
        out.push(("actors", Value::List(actors)));
    }
    if let Some(c) = channel {
        out.push(("channel", Value::Text(c)));
    }
    if let Some(n) = rating {
        out.push(("rating", Value::Text(n.to_string())));
    }
    if !tags.is_empty() {
        out.push(("tags", Value::List(tags)));
    }
    out
}

/// Drop every `[...]` block. It is the spec block (`[1080p 30fps h264]`) in a
/// footage name and free-form notes elsewhere; neither is a field. An
/// unclosed `[` is kept as text rather than swallowing the rest of the name.
fn strip_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        match rest[open..].find(']') {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &rest[open + close + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `YYYY-MM-DD`, optionally followed by `--HH-MM-SS` (rename-footage) or
/// `THH:MM:SS`, at the head of the stem. Returns the ISO form the Date
/// control accepts without a warning, and what follows it.
fn take_date(s: &str) -> Option<(String, &str)> {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| b.len() >= from + n && b[from..from + n].iter().all(u8::is_ascii_digit);
    if !(digits(0, 4) && b.get(4) == Some(&b'-') && digits(5, 2) && b.get(7) == Some(&b'-') && digits(8, 2)) {
        return None;
    }
    let date = &s[..10];
    // rename-footage: `2024-05-01--13-22-08`.
    if s[10..].starts_with("--") && digits(12, 2) && b.get(14) == Some(&b'-') && digits(15, 2) && b.get(17) == Some(&b'-') && digits(18, 2) {
        let stamp = format!("{date}T{}:{}:{}", &s[12..14], &s[15..17], &s[18..20]);
        return Some((stamp, s[20..].trim_start()));
    }
    // ISO: `2024-05-01T13:22:08`.
    if b.get(10) == Some(&b'T') && digits(11, 2) && b.get(13) == Some(&b':') && digits(14, 2) && b.get(16) == Some(&b':') && digits(17, 2) {
        return Some((s[..19].to_string(), s[19..].trim_start()));
    }
    // A bare date must end at a word boundary: `2024-05-01x` is not a date.
    match s[10..].chars().next() {
        None => Some((date.to_string(), "")),
        Some(c) if c.is_whitespace() => Some((date.to_string(), s[10..].trim_start())),
        _ => None,
    }
}

/// `Actor A, Actor B (Channel)` → the actors and the channel. The channel is
/// the trailing parenthesised group and only that; a `(` earlier in a name is
/// somebody's own punctuation.
fn split_people(p: &str) -> (Vec<String>, Option<String>) {
    let (names, channel) = match (p.ends_with(')'), p.rfind('(')) {
        (true, Some(open)) => {
            let ch = p[open + 1..p.len() - 1].trim();
            (&p[..open], (!ch.is_empty()).then(|| ch.to_string()))
        }
        _ => (p, None),
    };
    let actors = names
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (actors, channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(out: &'a [(&str, Value)], id: &str) -> Option<&'a Value> {
        out.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
    }
    fn text(out: &[(&str, Value)], id: &str) -> Option<String> {
        match get(out, id) {
            Some(Value::Text(s)) => Some(s.clone()),
            _ => None,
        }
    }
    fn list(out: &[(&str, Value)], id: &str) -> Vec<String> {
        match get(out, id) {
            Some(Value::List(l)) => l.clone(),
            _ => Vec::new(),
        }
    }

    /// The full media shape, stars last.
    #[test]
    fn actors_channel_title_tags_and_stars() {
        let out = parse("Ann Lee, Bo Cruz (Studio X) - A Title #pov #hd ★★★★☆");
        assert_eq!(list(&out, "actors"), ["Ann Lee", "Bo Cruz"]);
        assert_eq!(text(&out, "channel").as_deref(), Some("Studio X"));
        assert_eq!(text(&out, "title").as_deref(), Some("A Title"));
        assert_eq!(list(&out, "tags"), ["pov", "hd"]);
        assert_eq!(text(&out, "rating").as_deref(), Some("4"));
        assert!(get(&out, "date").is_none());
    }

    /// The meta block is never a field, wherever it sits in the name.
    #[test]
    fn a_meta_block_is_dropped_before_or_after_the_stars() {
        for stem in [
            "Ann (Ch) - T #a [1080p 30fps] ★★★☆☆",
            "Ann (Ch) - T #a ★★★☆☆ [1080p 30fps]",
            "Ann (Ch) - T [x] #a ★★★☆☆",
        ] {
            let out = parse(stem);
            assert_eq!(text(&out, "title").as_deref(), Some("T"), "{stem}");
            assert_eq!(list(&out, "tags"), ["a"], "{stem}");
            assert_eq!(text(&out, "rating").as_deref(), Some("3"), "{stem}");
            assert!(!out.iter().any(|(_, v)| format!("{v:?}").contains("1080p")), "{stem}");
        }
    }

    /// Only ★ counts; ☆ is the empty slot and is discarded, and a run of
    /// nothing but ☆ still says "zero stars".
    #[test]
    fn only_filled_stars_count() {
        assert_eq!(text(&parse("T ★☆☆☆☆"), "rating").as_deref(), Some("1"));
        assert_eq!(text(&parse("T ☆☆☆☆☆"), "rating").as_deref(), Some("0"));
        assert_eq!(text(&parse("T ★★★★★"), "rating").as_deref(), Some("5"));
        assert!(get(&parse("T"), "rating").is_none());
        assert_eq!(text(&parse("T ★★☆☆☆"), "title").as_deref(), Some("T"));
    }

    #[test]
    fn one_actor_with_and_without_a_channel() {
        let out = parse("Ann (Ch) - T #a");
        assert_eq!(list(&out, "actors"), ["Ann"]);
        assert_eq!(text(&out, "channel").as_deref(), Some("Ch"));
        let out = parse("Ann - T #a");
        assert_eq!(list(&out, "actors"), ["Ann"]);
        assert!(get(&out, "channel").is_none());
    }

    /// The footage shape: a timestamp leads, in rename-footage's own spelling
    /// and in ISO, and comes back as the instant the Date control accepts.
    #[test]
    fn a_leading_timestamp_is_the_date() {
        let out = parse("2024-05-01--13-22-08 Ann - T #a [1080p]");
        assert_eq!(text(&out, "date").as_deref(), Some("2024-05-01T13:22:08"));
        assert_eq!(list(&out, "actors"), ["Ann"]);
        assert_eq!(text(&out, "title").as_deref(), Some("T"));
        let out = parse("2024-05-01T13:22:08 Ann - T");
        assert_eq!(text(&out, "date").as_deref(), Some("2024-05-01T13:22:08"));
        let out = parse("2024-05-01 Ann - T");
        assert_eq!(text(&out, "date").as_deref(), Some("2024-05-01"));
        assert_eq!(list(&out, "actors"), ["Ann"]);
        // Not a date: the digits run straight into a word.
        let out = parse("2024-05-01x - T");
        assert!(get(&out, "date").is_none());
        assert_eq!(list(&out, "actors"), ["2024-05-01x"]);
    }

    /// No ` - ` means no people: the whole stem is the title, minus the
    /// tokens that are never title.
    #[test]
    fn a_name_without_a_dash_is_all_title() {
        let out = parse("Just a title #tag ★★☆☆☆");
        assert_eq!(text(&out, "title").as_deref(), Some("Just a title"));
        assert!(get(&out, "actors").is_none());
        assert_eq!(list(&out, "tags"), ["tag"]);
        // A dash inside the title, after the first, stays in it.
        let out = parse("Ann - T - part 2");
        assert_eq!(text(&out, "title").as_deref(), Some("T - part 2"));
    }

    /// Tags go through the same repair the field applies, and are unique.
    #[test]
    fn tags_are_repaired_and_deduplicated() {
        let out = parse("Ann - T #this-is-a-tag #example_tag #Tag #tag #");
        assert_eq!(list(&out, "tags"), ["this-is-a-tag", "example-tag", "Tag"]);
    }

    #[test]
    fn the_extension_is_dropped_from_a_path() {
        let out = parse_path(Path::new("/x/Ann (Ch) - T #a ★★★★★.mp4"));
        assert_eq!(text(&out, "title").as_deref(), Some("T"));
        assert_eq!(text(&out, "rating").as_deref(), Some("5"));
    }

    #[test]
    fn an_empty_or_meta_only_name_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
        assert!(parse("[1080p]").is_empty());
    }
}
