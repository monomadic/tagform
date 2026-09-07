//! The keymap, once (DESIGN §11).
//!
//! §11 says the full map lives in `--help` and is generated from one source.
//! It was not: `--help` held a hand-written block and the shortcut strip held
//! its own abbreviations, so `?` would have made a third copy of the same
//! facts and the third one to go stale. This table is that single source --
//! `--help` prints it, the `?` overlay paints it, and the strip stays an
//! abbreviation of it by hand because a one-line strip is a different job.

/// One binding: the keys as the user presses them, and what they do.
pub struct Bind {
    pub keys: &'static str,
    pub what: &'static str,
}

/// A run of bindings under a heading. `note` is the one sentence that says
/// when the section's keys are live at all -- which for a modal form is the
/// thing that makes the rest legible.
pub struct Section {
    pub title: &'static str,
    pub note: &'static str,
    pub binds: &'static [Bind],
}

const fn b(keys: &'static str, what: &'static str) -> Bind {
    Bind { keys, what }
}

pub const KEYMAP: &[Section] = &[
    Section {
        title: "SELECT",
        note: "the default mode: moves and commands, nothing types",
        binds: &[
            b("j k ↑ ↓ ⇥", "move between fields"),
            b("g G", "first / last field"),
            b("h l ← →", "step a fixed set, or nudge a rating"),
            b("⏎", "edit the focused field — on an empty date, fill in now"),
            b("w", "write staged edits (shows a plan to confirm first)"),
            b("^s", "write from either mode, committing the open field first"),
            b("r", "rename the file(s) in view from their tags on disk"),
            b("d", "on the URL field: fetch the page's tags with yt-dlp (no download)"),
            b("m", "merge a list field across every file in the selection"),
            b("i", "inspector — per-file values for the focused field"),
            b("] [", "next / previous file"),
            b("a", "all files — back to the aggregate view"),
            b("o", "open the file in the desktop player"),
            b("O", "overwrite the focused field on every open file"),
            b("b", "backfill it into only the files where it is still empty"),
            b("u ^r", "undo / redo"),
            b("⌫", "clear the focused field"),
            b("y c p", "yank the focused field (c copies too) / paste into it"),
            b("f", "format menu — then c capitalize, t title, l lower, u upper"),
            b("t", "cycle the colour scheme"),
            b("F", "toggle MOV faststart on the write"),
            b("?", "this key map, inside the form"),
            b("q esc", "quit (asks if edits are staged)"),
        ],
    },
    Section {
        title: "EDIT",
        note: "a field is open; keys the control does not want fall through",
        binds: &[
            b("(type)", "edit the field"),
            b("← →", "adjust a rating"),
            b("⏎", "save and stop editing"),
            b("⇥ ⇧⇥", "save and move to the next / previous field"),
            b("j k ↑ ↓", "save and move a row, on a control with no text to type"),
            b("esc", "cancel this field's edit"),
            b("^s", "save the field and write"),
            b("^c", "quit, from either mode"),
        ],
    },
    Section {
        title: "EDIT · text fields",
        note: "the emacs/macOS keys, bound only while a field is open",
        binds: &[
            b("^a ^e", "start / end of line"),
            b("^b ^f", "back / forward one character"),
            b("^d ^h", "delete the character right / left"),
            b("^w", "delete the word behind the cursor"),
            b("^k", "delete to end of line"),
            b("^u", "clear the line"),
        ],
    },
];

/// Columns a key legend occupies. `⏎` and `⌫` are one *character* and two
/// *columns* in most terminals, and padding by character count is what makes a
/// table of them lean.
pub fn key_width(keys: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(keys)
}

/// The map as plain text for `--help`, where there is no theme and no width to
/// measure -- so the key column is padded to the widest key in the whole table
/// and every section lines up with every other.
pub fn usage() -> String {
    let width = KEYMAP.iter().flat_map(|s| s.binds.iter()).map(|k| key_width(k.keys)).max().unwrap_or(0);
    let mut out = String::new();
    for s in KEYMAP {
        out.push_str(&format!("\n  {} — {}\n", s.title, s.note));
        for k in s.binds {
            let pad = " ".repeat(width - key_width(k.keys));
            out.push_str(&format!("    {}{pad}   {}\n", k.keys, k.what));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?` is the only key that documents the rest, so a table that forgot to
    /// list it would leave the overlay undiscoverable from inside itself.
    #[test]
    fn the_help_key_is_in_the_map() {
        assert!(KEYMAP.iter().flat_map(|s| s.binds.iter()).any(|k| k.keys == "?"));
    }

    #[test]
    fn usage_lines_up_and_names_every_section() {
        let u = usage();
        for s in KEYMAP {
            assert!(u.contains(s.title), "{} missing from --help", s.title);
        }
        assert!(u.contains("q esc"));
    }
}
