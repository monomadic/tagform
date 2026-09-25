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
//! Actor, Actor (Channel) - Title #tag #tag @G ★★★★☆
//! Actor, Actor (Channel) - Title #tag #tag [meta] ★★★★☆
//! Actor (Channel) - Title #tag #tag [meta]
//! Actor - Title #tag #tag [meta]
//! 2024-05-01--13-22-08 Actor - Title #tag #tag [meta]
//! Title #tag ★★★☆☆
//! ```
//!
//! The parts are pulled off in the order that makes each one unambiguous: a
//! `[...]` block is the probed spec `rename-footage` appends and is never a
//! field, so it goes first; stars, `#tags` and the `@G`/`@L`/`@T` orientation
//! mark are single tokens anywhere in the stem; a leading timestamp is a fixed shape; and only then is what remains
//! split on the first ` - ` into the people (with their `(Channel)`) and the
//! title. A name with no ` - ` is all title, because guessing that a plain
//! word is an actor is the wrong kind of helpful.
//!
//! Pure: a stem in, field values out, in the same list shape `fetch.rs`
//! returns, so the app stages both through one path. A field the name does
//! not carry is not in the list; what the caller does about a field that is
//! already filled is the caller's rule (the import never overwrites).
//!
//! One field cannot be read from a name on its own: a clip's Track number is
//! only a number until you see the names beside it. `track_sequence` takes the
//! whole batch for that reason, and is the one function here that does.

use std::path::Path;

use crate::config::ORIENTATION_MARKS;
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
    // `tag-two` the form would have written. The orientation mark is a token
    // of the same kind, matched exactly: `@G`, not `@g` and not `@Gmail`.
    let mut tags: Vec<String> = Vec::new();
    let mut orientation: Option<&'static str> = None;
    let mut kept: Vec<&str> = Vec::new();
    for tok in rest.split_whitespace() {
        if tok.len() > 1 && tok.starts_with('#') {
            let t = tag::repair(tok);
            if !t.is_empty() && !tags.iter().any(|x| x.eq_ignore_ascii_case(&t)) {
                tags.push(t);
            }
        } else if let Some((name, _)) = ORIENTATION_MARKS.iter().find(|(_, m)| *m == tok) {
            orientation = orientation.or(Some(name));
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
    if let Some(o) = orientation {
        out.push(("orientation", Value::Text(o.to_string())));
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


/// A digit run longer than this is not a clip number. Four digits is where the
/// numbers that are *never* a track live -- `2019`, `1080`, `2160` -- and
/// excluding them by shape is what keeps a year that happens to climb across a
/// batch from being read as the sequence.
const MAX_TRACK_DIGITS: usize = 3;

/// The number each name in a batch carries as its place in a sequence, one per
/// stem in the order given, or `None` if the batch does not spell one out.
///
/// The shape this recognises is a directory of cuts from one work:
///
/// ```text
/// birds 2019 avary in the wind 01.mp4
/// birds 2019 cherry big 02.mp4
/// birds 2019 03.mp4
/// ```
///
/// Every name holds two numbers; only one of them climbs. So the candidates
/// are the digit runs at one *slot* -- counted from the front of the name's
/// runs, and from the back, since the count of runs per name varies -- and a
/// slot qualifies when every name has a run there and the values strictly
/// increase through the batch. `2019` is excluded before that by
/// `MAX_TRACK_DIGITS`.
///
/// "Through the batch" is the order given -- the order the batch is listed in,
/// where the climb is the thing you can see -- or, failing that, natural
/// filename order, which is what rescues an unpadded `clip 9`, `clip 10` that
/// a shell glob handed over as 10 before 9. Either will do: each name's number
/// is read from that name, so the order only ever decides *which* number is
/// the sequence, never which file gets what.
///
/// Two slots that both climb, to different sequences, is an ambiguous batch
/// and yields nothing. The number is the caller's to compare against what the
/// files already hold -- this says only what the names say.
pub fn track_sequence(stems: &[&str]) -> Option<Vec<u32>> {
    // One name is not a sequence: there is nothing for it to increase against.
    if stems.len() < 2 {
        return None;
    }
    let runs: Vec<Vec<u32>> = stems.iter().map(|s| numbers(s)).collect();
    let given: Vec<usize> = (0..stems.len()).collect();
    let mut natural = given.clone();
    natural.sort_by(|a, b| natural_cmp(stems[*a], stems[*b]));
    let climbs = |vals: &[u32], order: &[usize]| order.windows(2).all(|w| vals[w[0]] < vals[w[1]]);

    let mut found: Vec<Vec<u32>> = Vec::new();
    for slot in 0..runs.iter().map(Vec::len).max()? {
        for from_end in [false, true] {
            let Some(vals) = at_slot(&runs, slot, from_end) else { continue };
            if !climbs(&vals, &given) && !climbs(&vals, &natural) {
                continue;
            }
            if !found.contains(&vals) {
                found.push(vals);
            }
        }
    }
    match found.len() {
        1 => found.pop(),
        _ => None,
    }
}

/// The value every name holds at one slot, counted from the front of its runs
/// or from the back. `None` if any name is short of that slot -- a sequence
/// one file is not part of is not a sequence over this batch.
fn at_slot(runs: &[Vec<u32>], slot: usize, from_end: bool) -> Option<Vec<u32>> {
    runs.iter()
        .map(|r| {
            let at = if from_end { r.len().checked_sub(slot + 1)? } else { slot };
            r.get(at).copied()
        })
        .collect()
}

/// The maximal runs of digits in a stem, in order, dropping the ones too long
/// to be a track number. Maximal first and filtered after, so `2019` is gone
/// rather than read as a `201` and a `9`.
fn numbers(stem: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = stem;
    while let Some(start) = rest.find(|c: char| c.is_ascii_digit()) {
        let run = &rest[start..];
        let end = run.find(|c: char| !c.is_ascii_digit()).unwrap_or(run.len());
        if end <= MAX_TRACK_DIGITS {
            if let Ok(n) = run[..end].parse::<u32>() {
                out.push(n);
            }
        }
        rest = &run[end..];
    }
    out
}

/// Order two stems the way a file browser lists them: digit runs compare as
/// numbers and everything else byte by byte, so `clip 9` comes before
/// `clip 10`. Plain lexicographic order puts `10` first, which would read an
/// unpadded sequence as decreasing and refuse it.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let (nx, rx) = take_number(x);
                let (ny, ry) = take_number(y);
                if nx != ny {
                    return nx.cmp(&ny);
                }
                (x, y) = (rx, ry);
            }
            (Some(p), Some(q)) if p != q => return p.cmp(q),
            _ => (x, y) = (&x[1..], &y[1..]),
        }
    }
}

/// The leading digit run as a number, and what follows it. Saturating: a run
/// of forty digits is not a number anyone is sequencing by, and only its
/// relative order matters here.
fn take_number(s: &[u8]) -> (u128, &[u8]) {
    let end = s.iter().position(|b| !b.is_ascii_digit()).unwrap_or(s.len());
    let n = s[..end].iter().fold(0u128, |acc, b| {
        acc.saturating_mul(10).saturating_add(u128::from(b - b'0'))
    });
    (n, &s[end..])
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

    /// The orientation mark is a token beside the tags, one per marked
    /// ORIENTATIONS value; Straight has none and a name without one says
    /// nothing about the field.
    #[test]
    fn the_orientation_mark_is_a_field() {
        for (name, mark) in ORIENTATION_MARKS {
            let stem = format!("Ann (Ch) - T #a {mark} ★★★☆☆ [1080p 30fps H]");
            let out = parse(&stem);
            assert_eq!(text(&out, "orientation").as_deref(), Some(*name), "{stem}");
            assert_eq!(text(&out, "title").as_deref(), Some("T"), "{stem}");
            assert_eq!(list(&out, "tags"), ["a"], "{stem}");
        }
        assert!(get(&parse("Ann - T #a [1080p 30fps H]"), "orientation").is_none());
        // Exact tokens only: case and length both matter, and a title keeps
        // its `@`-words.
        let out = parse("Ann - see you @ 7 @g @Gmail");
        assert!(get(&out, "orientation").is_none());
        assert_eq!(text(&out, "title").as_deref(), Some("see you @ 7 @g @Gmail"));
    }

    #[test]
    fn the_extension_is_dropped_from_a_path() {
        let out = parse_path(Path::new("/x/Ann (Ch) - T #a ★★★★★.mp4"));
        assert_eq!(text(&out, "title").as_deref(), Some("T"));
        assert_eq!(text(&out, "rating").as_deref(), Some("5"));
    }

    /// The batch the feature exists for: one number climbs through the names,
    /// and the other -- a year, in every name and never moving -- does not.
    #[test]
    fn a_climbing_number_in_a_batch_of_names_is_the_track() {
        let stems = [
            "birds 2019 avary in the wind 01",
            "birds 2019 cherry big 02",
            "birds 2019 03",
        ];
        assert_eq!(track_sequence(&stems), Some(vec![1, 2, 3]));
    }

    /// Counted from the back of each name's numbers as well as the front, so
    /// a name with an extra number in front of the sequence still lines up.
    #[test]
    fn the_slot_is_found_from_either_end() {
        assert_eq!(track_sequence(&["s01 clip 1", "clip 2", "s01 clip 3"]), Some(vec![1, 2, 3]));
    }

    /// Natural order, not byte order: unpadded numbers run 9, 10, 11 rather
    /// than 10, 11, 9, and byte order would read that as decreasing.
    #[test]
    fn an_unpadded_sequence_is_read_in_natural_order() {
        assert_eq!(track_sequence(&["clip 9", "clip 10", "clip 11"]), Some(vec![9, 10, 11]));
        // Whatever order the caller passes them in: each name still gives its
        // own number, and only which slot is the sequence was in question.
        assert_eq!(track_sequence(&["clip 11", "clip 9", "clip 10"]), Some(vec![11, 9, 10]));
        assert_eq!(track_sequence(&["clip 03", "clip 02", "clip 01"]), Some(vec![3, 2, 1]));
    }

    /// What must not become a track: a number that never moves, a number
    /// missing from one of the names, a repeat, and a batch of one.
    #[test]
    fn a_batch_without_one_climbing_number_yields_nothing() {
        assert_eq!(track_sequence(&["birds 2019 a", "birds 2019 b"]), None);
        assert_eq!(track_sequence(&["clip 01", "clip 02", "clip"]), None);
        assert_eq!(track_sequence(&["clip 01", "clip 01"]), None);
        assert_eq!(track_sequence(&["clip 01"]), None);
        assert_eq!(track_sequence(&[]), None);
    }

    /// Two slots that both climb cannot both be the track number, and picking
    /// one of them would be a guess. The names have to be unambiguous.
    #[test]
    fn an_ambiguous_batch_yields_nothing() {
        assert_eq!(track_sequence(&["a 1 5", "a 2 6", "a 3 7"]), None);
    }

    /// A four-digit run is a year or a resolution, never a clip number -- and
    /// it is dropped whole, not read as the three digits it opens with.
    #[test]
    fn long_digit_runs_are_not_track_numbers() {
        assert_eq!(numbers("birds 2019 03"), vec![3]);
        assert_eq!(numbers("1080p 720p"), vec![720]);
        // A year that climbs is still not a track number.
        assert_eq!(track_sequence(&["a 2019", "a 2020"]), None);
    }

    #[test]
    fn an_empty_or_meta_only_name_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
        assert!(parse("[1080p]").is_empty());
    }
}
