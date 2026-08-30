//! Every `§N` citation must resolve to a real heading in DESIGN.md.
//!
//! The citations are the thing that makes a 1500-line design document usable
//! from a 200-line module: `write.rs` says "DESIGN §9.2" instead of restating
//! the argument. That only works while they resolve, and nothing about editing
//! a heading tells you which files pointed at it -- so a renamed or renumbered
//! section rots its citations silently, which is exactly how §17.2..17.5 came
//! to dangle before this test existed.
//!
//! With this check, renumbering is a safe mechanical edit rather than a risk,
//! which is the reason the document can keep plain ordinal sections instead of
//! an insertion-proof numbering scheme.

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Heading numbers DESIGN.md actually defines: `## 9.`, `### 9.2`, `#### 9.2.1`.
fn headings(doc: &str) -> Vec<String> {
    doc.lines()
        .filter_map(|l| l.strip_prefix("##"))
        .map(|l| l.trim_start_matches('#').trim())
        .filter_map(|l| l.split_whitespace().next())
        .map(|n| n.trim_end_matches('.').to_string())
        .filter(|n| n.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .collect()
}

/// Section numbers cited in `text`, with the byte offset of each, so a failure
/// can point at the line. `skip_marker` drops citations belonging to another
/// document -- "docs/CONTAINER.md §4" is not a claim about this one.
fn citations(text: &str, marker: &str, skip_near: &[&str]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices(marker) {
        // Back up to a char boundary: the layout sketch is full of
        // box-drawing glyphs, and a blind byte offset lands inside one.
        let mut from = at.saturating_sub(40);
        while from < at && !text.is_char_boundary(from) {
            from += 1;
        }
        let before = &text[from..at];
        if skip_near.iter().any(|s| before.contains(s)) {
            continue;
        }
        let rest = &text[at + marker.len()..];
        let n: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let n = n.trim_end_matches('.');
        if !n.is_empty() {
            out.push((at, n.to_string()));
        }
    }
    out
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

fn rust_sources(dir: &Path, into: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).expect("reading src").flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, into);
        } else if p.extension().is_some_and(|x| x == "rs") {
            into.push(p);
        }
    }
}

#[test]
fn every_design_citation_resolves() {
    let doc_path = root().join("DESIGN.md");
    let doc = fs::read_to_string(&doc_path).expect("reading DESIGN.md");
    let defined = headings(&doc);
    assert!(defined.len() > 15, "no headings parsed; the format must have changed");

    let mut dangling: Vec<String> = Vec::new();

    // Cross-references inside the document itself.
    for (at, n) in citations(&doc, "§", &["CONTAINER"]) {
        if !defined.contains(&n) {
            dangling.push(format!("DESIGN.md:{} cites §{n}", line_of(&doc, at)));
        }
    }

    // And the module-level docs that point into it.
    let mut files = Vec::new();
    rust_sources(&root().join("src"), &mut files);
    assert!(!files.is_empty(), "found no sources to check");
    for f in files {
        let text = fs::read_to_string(&f).expect("reading a source file");
        // Both spellings: "DESIGN §9.2" and the "§9.1, §9.2" continuation.
        let mut found = citations(&text, "DESIGN §", &[]);
        found.extend(citations(&text, "§", &["CONTAINER"]));
        found.sort();
        found.dedup();
        for (at, n) in found {
            if !defined.contains(&n) {
                let rel = f.strip_prefix(root()).unwrap_or(&f).display();
                dangling.push(format!("{rel}:{} cites §{n}", line_of(&text, at)));
            }
        }
    }

    assert!(
        dangling.is_empty(),
        "citations pointing at sections DESIGN.md does not define:\n  {}",
        dangling.join("\n  ")
    );
}
