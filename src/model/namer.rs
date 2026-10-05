//! Composing a filename from a file's fields (DESIGN §9.4, the composing half).
//!
//! `rename-video` composes these names for MP4 and MOV; this is the same
//! grammar for the files that script cannot read, so a library does not end up
//! with two dialects. `filename.rs` is the inverse: what one writes, the other
//! must read back.
//!
//! The grammar is data, not code. A [`Template`] is a string, and the two
//! shipped ones ([`ADULT`], [`FOOTAGE`]) are ordinary templates that a config
//! file can replace one day without touching this module:
//!
//! ```text
//! {name}      the variable, or nothing when the file has no value for it
//! <...>       a group, kept only when every variable inside it has a value
//! \< \> \{ \} \\   the character itself
//! ```
//!
//! Anything else is literal. The variables are [`VARS`]. Two of them are
//! composed rather than read, because the grammar has punctuation that depends
//! on what is present and a template has no way to say it: `{headline}` is the
//! people and title with the dash only when both sides exist (and a channel
//! that is also an actor left out), and `{meta}` is the probed spec in the
//! order its category has always used, since the names already on disk in
//! their thousands set that order.
//!
//! Pure: fields in, a name out. Gathering the fields from a file is the
//! caller's (`tags/rename.rs`), which is what keeps this testable without
//! ffprobe.

use crate::config::ORIENTATION_MARKS;

/// The longest name written, in bytes. Every common filesystem holds 255, but
/// a name that fills them is one a Finder column, a listing and a share link
/// all truncate; the slack leaves room for a counter or another extension.
pub const NAME_MAX: usize = 220;

/// `ACTORS (CHANNEL) - TITLE (Clip N) #TAGS @G ★★★☆☆ [METADATA]`
pub const ADULT: &str =
    "{headline}< (Clip {clip})>< {tags}>< {orientation}>< {stars}> [{meta}]";

/// `DATE PEOPLE - TITLE (Clip N) (LOCATION) #TAGS ★★★☆☆ [METADATA]`
pub const FOOTAGE: &str = "{headline}< (Clip {clip})>< ({location})>< {tags}>< {stars}> [{meta}]";

/// Every name a template may use.
pub const VARS: &[&str] = &[
    "headline",
    "actors",
    "channel",
    "title",
    "clip",
    "tags",
    "orientation",
    "stars",
    "location",
    "date",
    "meta",
    "category",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Lit(String),
    Var(String),
    Group(Vec<Node>),
}

/// A parsed template. Parse once, render per file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template(Vec<Node>);

impl Template {
    pub fn parse(src: &str) -> Result<Self, String> {
        let mut top: Vec<Node> = Vec::new();
        let mut group: Option<Vec<Node>> = None;
        let mut lit = String::new();
        let mut chars = src.chars();

        fn flush(lit: &mut String, into: &mut Vec<Node>) {
            if !lit.is_empty() {
                into.push(Node::Lit(std::mem::take(lit)));
            }
        }

        while let Some(c) = chars.next() {
            match c {
                '\\' => lit.push(chars.next().ok_or("a template cannot end in a backslash")?),
                '{' => {
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) => name.push(c),
                            None => return Err("`{` is never closed".into()),
                        }
                    }
                    if !VARS.contains(&name.as_str()) {
                        return Err(format!("unknown variable `{{{name}}}`"));
                    }
                    flush(&mut lit, group.as_mut().unwrap_or(&mut top));
                    group.as_mut().unwrap_or(&mut top).push(Node::Var(name));
                }
                '<' => {
                    if group.is_some() {
                        return Err("groups do not nest".into());
                    }
                    flush(&mut lit, &mut top);
                    group = Some(Vec::new());
                }
                '>' => {
                    let Some(mut g) = group.take() else {
                        return Err("`>` closes nothing".into());
                    };
                    flush(&mut lit, &mut g);
                    top.push(Node::Group(g));
                }
                c => lit.push(c),
            }
        }
        if group.is_some() {
            return Err("`<` is never closed".into());
        }
        flush(&mut lit, &mut top);
        Ok(Template(top))
    }

    fn render(&self, vars: &Vars) -> String {
        fn run(nodes: &[Node], vars: &Vars, out: &mut String) {
            for n in nodes {
                match n {
                    Node::Lit(s) => out.push_str(s),
                    Node::Var(v) => out.push_str(vars.get(v)),
                    Node::Group(g) => {
                        let complete = g
                            .iter()
                            .all(|n| !matches!(n, Node::Var(v) if vars.get(v).is_empty()));
                        if complete {
                            run(g, vars, out);
                        }
                    }
                }
            }
        }
        let mut out = String::new();
        run(&self.0, vars, &mut out);
        out
    }
}

/// The two templates in use. `Default` is the shipped grammar.
#[derive(Debug, Clone)]
pub struct Templates {
    pub adult: Template,
    pub footage: Template,
}

impl Default for Templates {
    fn default() -> Self {
        Templates {
            adult: Template::parse(ADULT).expect("shipped template parses"),
            footage: Template::parse(FOOTAGE).expect("shipped template parses"),
        }
    }
}

struct Vars(Vec<(&'static str, String)>);

impl Vars {
    fn get(&self, name: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| *k == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    Adult,
    Footage,
    /// Any value with no grammar of its own, and no value at all. Named by
    /// the Adult grammar, which is the generic one.
    #[default]
    Other,
}

impl Category {
    /// `Camera Footage` is what the yt-dlp alias writes; `Footage` is what the
    /// form shows.
    pub fn from_tag(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "adult" | "porn" | "xxx" => Category::Adult,
            "footage" | "camera footage" => Category::Footage,
            _ => Category::Other,
        }
    }
}

/// The probed spec that ends a name. An empty string is absent.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub resolution: String,
    pub fps: String,
    pub duration: String,
    pub mbps: String,
    pub codec: String,
    pub device: String,
    /// `H`, `V` or `S`.
    pub shape: String,
}

#[derive(Debug, Clone, Default)]
pub struct Fields {
    pub category: Category,
    pub actors: Vec<String>,
    pub channel: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Raw: a number or a run of stars.
    pub rating: String,
    /// Set only for a Clip that carries a Track.
    pub clip: Option<u32>,
    pub orientation: String,
    pub location: String,
    /// `YYYY-MM-DD--HH-MM-SS`.
    pub date: String,
    /// Whether the file carries the URL it was fetched from.
    pub has_source: bool,
    pub spec: Spec,
}

impl Fields {
    /// Why the file may not be named yet, per its category: what a file must
    /// carry to prove it was tagged on purpose rather than left as it came.
    pub fn missing(&self) -> Option<&'static str> {
        match self.category {
            Category::Adult if !self.has_source && self.title.is_empty() => {
                Some("Adult needs a source URL or a title")
            }
            Category::Footage if self.title.is_empty() && self.date.is_empty() => {
                Some("Footage needs a date or a title")
            }
            Category::Other if !self.has_source => Some(
                "an uncategorised file needs a source URL (original_url, purl, source_url or a URL comment)",
            ),
            _ => None,
        }
    }

    fn headline(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.category == Category::Footage {
            if !self.date.is_empty() {
                parts.push(self.date.clone());
            }
            if !self.actors.is_empty() {
                parts.push(self.actors.join(", "));
            }
        } else if !self.actors.is_empty() {
            // A performer's own channel is already in the name as the actor.
            let own = self
                .actors
                .iter()
                .any(|a| a.to_lowercase() == self.channel.to_lowercase());
            let mut lead = self.actors.join(", ");
            if !self.channel.is_empty() && !own {
                lead.push_str(&format!(" ({})", self.channel));
            }
            parts.push(lead);
        } else if !self.channel.is_empty() {
            // The parentheses mark a channel as belonging to the names in
            // front of it; with none, there is nothing for them to do.
            parts.push(self.channel.clone());
        }
        if !self.title.is_empty() {
            if !parts.is_empty() {
                parts.push("-".into());
            }
            parts.push(self.title.clone());
        }
        parts.join(" ")
    }

    fn meta(&self) -> String {
        let s = &self.spec;
        // The two orders are the ones the old tools left on disk, and only
        // Footage names the device.
        let order: Vec<&String> = if self.category == Category::Footage {
            vec![&s.resolution, &s.fps, &s.duration, &s.mbps, &s.codec, &s.device, &s.shape]
        } else {
            vec![&s.resolution, &s.fps, &s.codec, &s.duration, &s.mbps, &s.shape]
        };
        order
            .into_iter()
            .filter(|v| !v.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn vars(&self) -> Vars {
        let mark = ORIENTATION_MARKS
            .iter()
            .find(|(o, _)| o.eq_ignore_ascii_case(self.orientation.trim()))
            .map_or("", |(_, m)| *m);
        Vars(vec![
            ("headline", self.headline()),
            ("actors", self.actors.join(", ")),
            ("channel", self.channel.clone()),
            ("title", self.title.clone()),
            ("clip", self.clip.map(|n| n.to_string()).unwrap_or_default()),
            (
                "tags",
                self.tags
                    .iter()
                    .map(|t| format!("#{}", t.trim_start_matches('#')))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            ("orientation", mark.to_string()),
            ("stars", stars(&self.rating)),
            ("location", self.location.clone()),
            ("date", self.date.clone()),
            ("meta", self.meta()),
            (
                "category",
                match self.category {
                    Category::Adult => "Adult",
                    Category::Footage => "Footage",
                    Category::Other => "Other",
                }
                .into(),
            ),
        ])
    }
}

/// A rating as exactly five `★`/`☆`, or nothing. It may be stored as a number
/// or as stars already; zero and unrated both render as nothing, because the
/// distinction lives in the tag and not in the name.
pub fn stars(raw: &str) -> String {
    let raw = raw.trim();
    let n = if raw.contains('★') {
        raw.chars().filter(|&c| c == '★').count()
    } else {
        raw.parse::<usize>().unwrap_or(0)
    }
    .min(5);
    if n == 0 {
        return String::new();
    }
    "★".repeat(n) + &"☆".repeat(5 - n)
}

fn tidy(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The stem for `f`, before any length limit. `None` when there is nothing to
/// name the file after: a name that is only a metadata block is not a name.
fn stem(f: &Fields, t: &Templates) -> Option<String> {
    if f.category != Category::Footage && f.headline().is_empty() {
        return None;
    }
    let template = if f.category == Category::Footage {
        &t.footage
    } else {
        &t.adult
    };
    // A tag may legitimately contain a slash.
    Some(tidy(&template.render(&f.vars())).replace('/', "-"))
}

/// The whole file name for `f`: the stem, the extension, and the length limit.
///
/// `current` is the name the file has now. Its leading underscores are kept,
/// being a hand-set mark that no tag records.
///
/// Past [`NAME_MAX`] the title goes first, losing words from its end -- the
/// tags are what a library is searched by, and a title reads fine cut short --
/// then the tags from the last, since the front of the list is the part a
/// person chose to lead with. People, rating and spec are left alone while
/// anything else can go. If they alone are still too long the stem is cropped
/// from its end, a character at a time: a cut name is still a name.
pub fn name(f: &Fields, t: &Templates, current: &str, ext: &str) -> Option<String> {
    let keep: String = current.chars().take_while(|&c| c == '_').collect();
    let dot = if ext.is_empty() { "" } else { "." };
    let mut f = f.clone();
    let mut s = stem(&f, t)?;
    let too_long = |s: &str| keep.len() + s.len() + dot.len() + ext.len() > NAME_MAX;
    while too_long(&s) {
        if let Some((head, _)) = f.title.rsplit_once(' ') {
            f.title = head.to_string();
        } else if !f.title.is_empty() {
            f.title.clear();
        } else if f.tags.pop().is_none() {
            break;
        }
        match stem(&f, t) {
            Some(next) => s = next,
            None => break,
        }
    }
    while too_long(&s) && !s.is_empty() {
        s.pop();
    }
    Some(format!("{keep}{}{dot}{ext}", s.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            resolution: "1080p".into(),
            fps: "30fps".into(),
            duration: "22min".into(),
            mbps: "8mbps".into(),
            codec: "h264".into(),
            device: String::new(),
            shape: "H".into(),
        }
    }

    fn adult() -> Fields {
        Fields {
            category: Category::Adult,
            actors: vec!["Alice".into(), "Bob".into()],
            channel: "Studio".into(),
            title: "Title".into(),
            tags: vec!["tag".into(), "other".into()],
            rating: "3".into(),
            spec: spec(),
            ..Default::default()
        }
    }

    fn named(f: &Fields) -> String {
        name(f, &Templates::default(), "x.mkv", "mkv").unwrap()
    }

    #[test]
    fn the_adult_grammar_in_full() {
        assert_eq!(
            named(&adult()),
            "Alice, Bob (Studio) - Title #tag #other ★★★☆☆ [1080p 30fps h264 22min 8mbps H].mkv"
        );
    }

    /// The four shapes the script's header lists, and the round trip: what is
    /// written here is what `filename::parse` reads back.
    #[test]
    fn segments_without_a_value_leave_no_punctuation() {
        let mut f = adult();
        f.actors.clear();
        assert!(named(&f).starts_with("Studio - Title #tag"));

        let mut f = adult();
        f.title.clear();
        assert!(named(&f).starts_with("Alice, Bob (Studio) #tag"));

        let mut f = adult();
        f.channel = "alice".into();
        assert!(named(&f).starts_with("Alice, Bob - Title"));

        let mut f = adult();
        f.actors.clear();
        f.channel.clear();
        f.title.clear();
        assert_eq!(name(&f, &Templates::default(), "x.mkv", "mkv"), None);
    }

    #[test]
    fn a_clip_carries_its_number_and_orientation_its_mark() {
        let mut f = adult();
        f.clip = Some(3);
        f.orientation = "Gay".into();
        assert!(named(&f).contains("Title (Clip 3) #tag #other @G ★★★☆☆ ["));
        f.orientation = "Straight".into();
        assert!(!named(&f).contains('@'));
    }

    #[test]
    fn footage_puts_the_date_first_and_the_device_last() {
        let f = Fields {
            category: Category::Footage,
            date: "2025-02-02--12-23-44".into(),
            actors: vec!["Sam".into()],
            title: "Beach".into(),
            location: "Lisbon".into(),
            spec: Spec {
                device: "iPhone15".into(),
                ..spec()
            },
            ..Default::default()
        };
        assert_eq!(
            named(&f),
            "2025-02-02--12-23-44 Sam - Beach (Lisbon) [1080p 30fps 22min 8mbps h264 iPhone15 H].mkv"
        );
    }

    #[test]
    fn stars_are_five_characters_or_nothing() {
        assert_eq!(stars("4"), "★★★★☆");
        assert_eq!(stars("★★"), "★★☆☆☆");
        assert_eq!(stars("9"), "★★★★★");
        assert_eq!(stars("0"), "");
        assert_eq!(stars(""), "");
    }

    #[test]
    fn requirements_follow_the_category() {
        let mut f = Fields::default();
        assert!(f.missing().unwrap().contains("source URL"));
        f.has_source = true;
        assert!(f.missing().is_none());
        f.category = Category::Footage;
        f.has_source = false;
        assert!(f.missing().is_some());
        f.date = "2025-01-01--00-00-00".into();
        assert!(f.missing().is_none());
    }

    #[test]
    fn leading_underscores_survive_and_slashes_do_not() {
        let mut f = adult();
        f.title = "AC/DC".into();
        let n = name(&f, &Templates::default(), "__old.mkv", "mkv").unwrap();
        assert!(n.starts_with("__Alice") && n.contains("AC-DC"));
    }

    #[test]
    fn a_long_name_loses_its_title_before_its_tags() {
        let mut f = adult();
        f.title = "word ".repeat(60).trim().to_string();
        let n = named(&f);
        assert!(n.len() <= NAME_MAX);
        assert!(n.contains("#tag #other"));
        assert!(n.contains("Alice, Bob (Studio) - word"));
    }

    #[test]
    fn a_template_is_a_string() {
        let t = Template::parse("{title}< by {channel}>\\<{clip}\\>").unwrap();
        let mut f = adult();
        assert_eq!(t.render(&f.vars()), "Title by Studio<>");
        f.channel.clear();
        assert_eq!(t.render(&f.vars()), "Title<>");
        assert!(Template::parse("{nope}").is_err());
        assert!(Template::parse("<{title}").is_err());
        assert!(Template::parse("<<{title}>>").is_err());
        assert!(Template::parse("{title").is_err());
    }
}
