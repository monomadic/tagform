//! The tag grammar (DESIGN §5.4): how a line of tags is split, repaired, and
//! judged.
//!
//! Tags round-trip through *filenames*, so a tag is one token: no spaces, no
//! underscores. That is a shape the user should never have to type by hand, so
//! most of what goes wrong here is repaired rather than reported —
//! `tag two` becomes `tag-two`, mirroring the yt-dlp config's
//! `--replace-in-metadata "tags" "[ _]+" "-"`.
//!
//! What is left over is the small set that cannot be repaired honestly: a
//! slash, a backslash, a colon, a leading dot. Those could be flattened to `-`
//! too, but a slash in a tag is nearly always a pasted path or a hierarchy the
//! user meant something by, and inventing a tag they did not write is worse
//! than refusing the field. So they are an `Error` — red in the form, and the
//! field is left out of the write rather than saved wrong.
//!
//! Lives outside `ui/` because the write path asks the same question the
//! control does, and both must get the same answer.

/// Split an edit line into repaired tags.
///
/// The separator is the comma whenever the line has one; only a line without
/// commas splits on whitespace. DESIGN §5.4 called for splitting on either,
/// always — but that reading turns `tag, tag two` into three tags silently,
/// which is the one outcome the user cannot see happening. With a comma in the
/// line the spaces inside a tag are the user's, and repairing them is what
/// they meant.
///
/// The tags come back sorted alphabetically: a tag set has no
/// order worth keeping, and a sorted one reads the same on every file. Both
/// sides of the form's "did this change" comparison come through here, so a
/// set stored out of order on disk is not an edit until something else is.
///
/// A tag written twice is kept once. Repair has already lowercased both, so
/// `#Tag` and `#tag` are the same tag here, as they are to everything that
/// reads a hashtag.
pub fn split(line: &str) -> Vec<String> {
    let parts: Vec<&str> = if line.contains(',') {
        line.split(',').collect()
    } else {
        line.split_whitespace().collect()
    };
    let mut tags: Vec<String> = parts
        .iter()
        .map(|p| repair(p))
        .filter(|p| !p.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// One tag, made into a single filename token: the leading `#` is presentation
/// and never stored, and every run of whitespace, `_` or `-` between two
/// characters becomes exactly one `-`. Lowercased, because a hashtag's case
/// carries nothing and two spellings of one tag are two tags to a filename
/// search. Idempotent, which is what lets a
/// control be seeded from its own value without staging a phantom edit.
pub fn repair(raw: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in raw.trim().trim_start_matches('#').chars() {
        if c.is_whitespace() || c == '_' || c == '-' {
            // Trailing separators are dropped by never being emitted: the dash
            // is written only once a character follows it.
            gap = !out.is_empty();
        } else {
            if gap {
                out.push('-');
                gap = false;
            }
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Filename-hostile, and not something a repair can guess at.
pub fn is_hostile(tag: &str) -> bool {
    tag.starts_with('.')
        || tag
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
}

/// The first tag a write must refuse, if any.
pub fn first_hostile(tags: &[String]) -> Option<&str> {
    tags.iter().find(|t| is_hostile(t)).map(String::as_str)
}

/// Why a value cannot be stored in a hashtag field — the sentence the form and
/// the write path both say.
pub fn why_invalid(tags: &[String]) -> Option<String> {
    first_hostile(tags)
        .map(|bad| format!("‘{bad}’ is not a valid tag: / \\ : and a leading . cannot be repaired"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        split(s)
    }

    /// The reported case: a comma-separated line whose tags contain spaces.
    #[test]
    fn spaces_inside_a_comma_separated_tag_are_repaired() {
        assert_eq!(
            v("tag, tag two, tag three, another"),
            ["another", "tag", "tag-three", "tag-two"]
        );
    }

    /// A line without commas is still a stream of hashtags, which is how a
    /// pasted `#a #b` has always arrived.
    #[test]
    fn a_line_without_commas_splits_on_whitespace() {
        for line in ["pov hd", "#pov #hd", "  pov   hd "] {
            assert_eq!(v(line), ["hd", "pov"], "line: {line}");
        }
    }

    #[test]
    fn underscores_and_runs_collapse_to_one_dash() {
        assert_eq!(v("tag__two, a - b, -c-"), ["a-b", "c", "tag-two"]);
    }

    /// Lowercased before the sort, so a capitalised tag lands among its
    /// neighbours rather than ahead of every lowercase one.
    #[test]
    fn tags_come_back_lowercased_and_sorted() {
        assert_eq!(
            v("zebra, Apple, mango, Big Dog, #POV"),
            ["apple", "big-dog", "mango", "pov", "zebra"]
        );
    }

    /// Duplicates are judged after repair, so two spellings of one tag are one
    /// tag; a multi-word tag is not a duplicate of its own first word.
    #[test]
    fn duplicates_are_kept_once() {
        assert_eq!(v("frog, frog"), ["frog"]);
        assert_eq!(v("frog frog #frog"), ["frog"]);
        assert_eq!(v("frog dog, big dog, frog"), ["big-dog", "frog", "frog-dog"]);
        assert_eq!(v("big dog, big_dog, big-dog"), ["big-dog"]);
        assert_eq!(v("Frog, frog, FROG, ant"), ["ant", "frog"]);
    }

    /// The separator rule, on the lines it was specified with.
    #[test]
    fn commas_decide_whether_a_space_is_inside_a_tag() {
        assert_eq!(
            v("frog, bulldog, camel toe, burger"),
            ["bulldog", "burger", "camel-toe", "frog"]
        );
        assert_eq!(
            v("frog bulldog camel toe burger"),
            ["bulldog", "burger", "camel", "frog", "toe"]
        );
        assert_eq!(v("frog, big bull dog"), ["big-bull-dog", "frog"]);
    }

    /// Repair has to be a fixed point, or seeding a control from its own value
    /// would stage an edit nobody made.
    #[test]
    fn repair_is_idempotent() {
        for s in ["tag two", "a__b", "#x", " - ", "", "a-b", "Tag Two", "İ"] {
            let once = repair(s);
            assert_eq!(repair(&once), once, "{s}");
        }
    }

    #[test]
    fn empties_are_dropped_rather_than_becoming_blank_tags() {
        assert_eq!(v("a,,  , b,"), ["a", "b"]);
        assert!(v("  ").is_empty());
        assert!(v("").is_empty());
    }

    #[test]
    fn filename_hostile_tags_are_not_repairable() {
        assert!(why_invalid(&v("a/b")).is_some());
        assert!(why_invalid(&v("c:\\x")).is_some());
        assert!(why_invalid(&v(".hidden")).is_some());
        assert!(why_invalid(&v("ok, fine-too")).is_none());
        // A dot inside a tag is only awkward at the front.
        assert!(why_invalid(&v("v1.2")).is_none());
    }
}
