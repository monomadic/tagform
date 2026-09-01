//! Enum sources (DESIGN §3.5).
//!
//! Category and Variant are not invented here. They are exactly the aliases in
//! `config/yt-dlp/config`, which write `meta_genre` and `meta_type` -- yt-dlp's
//! own variable names, unchanged by either of this repo's field renames:
//!
//! ```text
//! --alias footage '--embed-metadata --parse-metadata "Camera Footage:%(meta_genre)s"'
//! --alias clip    '--embed-metadata --parse-metadata "Clip:%(meta_type)s"'
//! ```
//!
//! Hard-coding the values would guarantee drift the first time an alias is
//! added, so the config is parsed instead. Failure is not fatal: the defaults
//! below stand in, because a tagger that will not start because a config file
//! moved is worse than one with a slightly stale dropdown.

use std::path::PathBuf;

pub const DEFAULT_CATEGORIES: &[&str] = &[
    "Adult",
    "Footage",
    "Karaoke",
    "Live Visual",
    "Music Video",
    "Tutorial",
    "Meme",
    "Texture",
];
pub const DEFAULT_VARIANTS: &[&str] = &["Clip", "Enhanced", "Original"];

/// The `stik` media kind: a closed set the Apple ecosystem actually reads.
pub const KINDS: &[(&str, &str)] = &[
    ("0", "Home Video"),
    ("1", "Normal"),
    ("2", "Audiobook"),
    ("6", "Music Video"),
    ("9", "Movie"),
    ("10", "TV Show"),
    ("21", "Podcast"),
];

pub struct Enums {
    pub category: Vec<String>,
    pub variant: Vec<String>,
}

impl Default for Enums {
    fn default() -> Self {
        Self {
            category: DEFAULT_CATEGORIES.iter().map(|s| s.to_string()).collect(),
            variant: DEFAULT_VARIANTS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl Enums {
    pub fn load() -> Self {
        match ytdlp_config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) => Self::from_ytdlp_config(&text),
            None => Self::default(),
        }
    }

    pub fn from_ytdlp_config(text: &str) -> Self {
        let mut me = Self {
            // yt-dlp's own variables are still `meta_genre` and `meta_type`;
            // only our fields changed name, and that config lives in another
            // repository. `meta_genre` feeds Category, not Genre -- these
            // literals were never styles.
            category: parse_alias_values(text, "meta_genre"),
            variant: parse_alias_values(text, "meta_type"),
        };
        if me.category.is_empty() {
            me.category = DEFAULT_CATEGORIES.iter().map(|s| s.to_string()).collect();
        }
        if me.variant.is_empty() {
            me.variant = DEFAULT_VARIANTS.iter().map(|s| s.to_string()).collect();
        }
        me
    }
}

fn ytdlp_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let p = base.join("yt-dlp/config");
    p.exists().then_some(p)
}

/// Pull every `"<VALUE>:%(<field>)s"` literal out of the alias lines.
fn parse_alias_values(text: &str, field: &str) -> Vec<String> {
    let needle = format!(":%({field})s");
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("--alias") {
            continue;
        }
        for (idx, _) in line.match_indices(&needle) {
            // Walk back from the marker to the quote that opens the literal.
            let before = &line[..idx];
            let Some(start) = before.rfind('"') else { continue };
            let value = normalize(unwrap_const(&before[start + 1..]));
            if !value.is_empty() && !out.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
                out.push(value);
            }
        }
    }
    out
}

/// The constant inside a yt-dlp `%(_const|VALUE)s` template, or the literal
/// unchanged.
///
/// Both spellings mean the same thing to yt-dlp and both appear in real
/// configs -- `"Media:%(meta_genre)s"` is the plain form, `_const` the one
/// yt-dlp's own docs now use. Reading only the plain form put the template
/// *source* in the dropdown: the form offered `%(_const|Media)s`, which then
/// matched nothing on disk, so no category ever lit.
fn unwrap_const(literal: &str) -> &str {
    let s = literal.trim();
    let Some(inner) = s.strip_prefix("%(").and_then(|s| s.strip_suffix(")s")) else {
        return s;
    };
    match inner.split_once('|') {
        // `%(_const|X)s` is X. Any other template is a per-file value, not a
        // constant, and names no member of the set -- drop it rather than
        // offering a placeholder that can never match.
        Some(("_const", v)) => v,
        _ => "",
    }
}

/// Canonical spelling for a value stored under an older name.
///
/// Applied in both directions: to the literals parsed out of the yt-dlp config,
/// so the dropdown offers the current name, and to values read off files
/// (`probe::normalize`), so a file tagged the old way displays as the new one.
/// The second half is what makes this a rename rather than a second entry in
/// the set -- without it an old value simply joins the list (§5.7) and the
/// dropdown offers both spellings of the same thing.
///
/// This is the read-wider-than-write rule (DESIGN §4.2) applied to values
/// rather than keys: understand the old spelling, only ever write the new one.
/// A file keeps its old value until the field is actually edited, which is why
/// no migration pass is needed.
pub fn normalize(value: &str) -> String {
    let v = value.trim();
    match v.to_ascii_lowercase().as_str() {
        "camera footage" => "Footage".to_string(),
        // "Media" said nothing: every file here is media. The value has always
        // meant the adult material this library was built around, so the label
        // now says it.
        "media" => "Adult".to_string(),
        // "VJ Clip" named the operator, not the thing -- and half of these do
        // not loop, so none of the shorter words fit either. A live visual is
        // image material played behind or over a performance: no narrative, no
        // runtime that matters, not a work you sit and watch.
        "vj clip" => "Live Visual".to_string(),
        // "Master" was ambiguous: in this library a master is the good copy,
        // but everywhere else it is the *source* -- the opposite of a
        // remastered or upscaled derivative, which is what this value means.
        "master" => "Enhanced".to_string(),
        _ => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# category (yt-dlp still calls the variable meta_genre)
--alias media '--embed-metadata --parse-metadata "Media:%(meta_genre)s"'
--alias footage '--embed-metadata --parse-metadata "Camera Footage:%(meta_genre)s"'
--alias karaoke '--embed-metadata --parse-metadata "Karaoke:%(meta_genre)s"'
--alias vj '--parse-metadata "VJ Clip:%(meta_genre)s"'

# type
--alias clip '--embed-metadata --parse-metadata "Clip:%(meta_type)s"'
--alias master '--embed-metadata --parse-metadata "Master:%(meta_type)s"'
--alias original '--embed-metadata --parse-metadata "Original:%(meta_type)s"'
"#;

    #[test]
    fn categories_come_from_the_aliases() {
        let e = Enums::from_ytdlp_config(SAMPLE);
        // The alias literals are still "Media" and "VJ Clip"; the form offers
        // "Adult" and "Live Visual".
        assert_eq!(e.category, vec!["Adult", "Footage", "Karaoke", "Live Visual"]);
    }

    #[test]
    fn variants_come_from_the_aliases() {
        let e = Enums::from_ytdlp_config(SAMPLE);
        // The alias literal is still "Master"; the form offers "Enhanced".
        assert_eq!(e.variant, vec!["Clip", "Enhanced", "Original"]);
    }

    /// The alias literal is "Camera Footage"; the form says "Footage".
    #[test]
    fn camera_footage_normalizes() {
        assert_eq!(normalize("Camera Footage"), "Footage");
        assert_eq!(normalize("camera footage"), "Footage");
        assert_eq!(normalize(" Karaoke "), "Karaoke");
        assert_eq!(normalize("Master"), "Enhanced");
        assert_eq!(normalize("MASTER"), "Enhanced");
        assert_eq!(normalize("VJ Clip"), "Live Visual");
        assert_eq!(normalize("Media"), "Adult");
        assert_eq!(normalize("media"), "Adult");
        assert_eq!(normalize("vj clip"), "Live Visual");
        // Already current, and anything unknown, passes through untouched.
        assert_eq!(normalize("Enhanced"), "Enhanced");
        assert_eq!(normalize("Upscale"), "Upscale");
    }

    /// A missing or unreadable config must not empty the dropdowns.
    #[test]
    fn empty_config_falls_back_to_defaults() {
        let e = Enums::from_ytdlp_config("");
        assert_eq!(e.category, DEFAULT_CATEGORIES);
        assert_eq!(e.variant, DEFAULT_VARIANTS);
    }

    /// The form the real config actually uses. Parsing this as a literal put
    /// `%(_const|Media)s` in the dropdown and lit nothing on any file.
    const CONST_SAMPLE: &str = r#"
--alias media '--embed-metadata --parse-metadata "%(_const|Media)s:%(meta_genre)s"'
--alias footage '--embed-metadata --parse-metadata "%(_const|Camera Footage)s:%(meta_genre)s"'
--alias vj '--parse-metadata "%(_const|VJ Clip)s:%(meta_genre)s"'
--alias clip '--embed-metadata --parse-metadata "%(_const|Clip)s:%(meta_type)s"'
--alias master '--embed-metadata --parse-metadata "%(_const|Master)s:%(meta_type)s"'
"#;

    #[test]
    fn const_templates_yield_their_constant() {
        let e = Enums::from_ytdlp_config(CONST_SAMPLE);
        assert_eq!(e.category, vec!["Adult", "Footage", "Live Visual"]);
        assert_eq!(e.variant, vec!["Clip", "Enhanced"]);
    }

    #[test]
    fn a_template_that_is_not_a_constant_names_no_value() {
        assert_eq!(unwrap_const("%(_const|Media)s"), "Media");
        assert_eq!(unwrap_const("Media"), "Media");
        // Not a constant: a per-file field, which can never be a set member.
        assert_eq!(unwrap_const("%(title)s"), "");
        assert_eq!(unwrap_const("%(uploader|unknown)s"), "");
        let e = Enums::from_ytdlp_config(
            "--alias x '--parse-metadata \"%(title)s:%(meta_genre)s\"'",
        );
        assert_eq!(e.category, DEFAULT_CATEGORIES);
    }

    #[test]
    fn non_alias_lines_are_ignored() {
        let e = Enums::from_ytdlp_config("--parse-metadata \"Bogus:%(meta_genre)s\"");
        assert_eq!(e.category, DEFAULT_CATEGORIES);
    }
}
