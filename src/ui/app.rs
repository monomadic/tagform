//! Application state and the event loop (DESIGN §6.3).
//!
//! Milestone 2 is read-only: the form displays and navigates, nothing is
//! edited and nothing is written. The focus ring, the aggregate/single-file
//! split and the inspector are all here because they are what the editing
//! milestones plug into.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{Enums, KINDS, ORIENTATIONS};
use crate::fetch;
use crate::geocode::{self, Hit};
use crate::model::filename;
use crate::model::schema::{
    self, field_by_id, Control, FieldDef, ADULT, ADULT_HIDDEN, ADULT_ORDER, CLIP, FIELDS, FOOTAGE,
    FOOTAGE_HIDDEN,
};
use crate::model::tag;
use crate::ui::edit::{Editor, Opt, Reaction, Validation};
use crate::ui::theme;
use crate::model::value::{Agg, Value};
use crate::tags::plan::{self, FilePlan};
use crate::tags::probe::FileTags;
use crate::tags::rename::{self, Outcome};
use crate::tags::write;
use crate::thumb::{self, MediaInfo};

/// One line in the form. A schema field, or -- below them -- a key found on
/// disk that no field claims. Custom keys get rows of their own so that an
/// unrecognised tag is visibly present rather than quietly missing.
pub struct Row {
    /// Stable across view changes and row rebuilds, so a staged edit stays
    /// attached to its field when the selection is re-aggregated.
    pub key: String,
    pub label: String,
    pub control: Control,
    pub def: Option<&'static FieldDef>,
    /// The aggregate as displayed: what is on disk with the staged edits laid
    /// over it, which is also what a write would leave behind. Disk truth is
    /// not carried here -- an edit is compared against the one file it is
    /// being staged on (`disk_value`), never against the selection.
    pub eff: Agg,
    /// Whether any file in scope carries a staged edit for this key.
    pub staged: bool,
}

impl Row {
    /// Mixed as displayed -- so a field the files disagreed about stops
    /// reading ‹multiple› once an edit has been staged across all of them.
    pub fn is_mixed(&self) -> bool {
        matches!(self.eff, Agg::Mixed { .. })
    }

    /// The value the row shows: the edit where there is one, else disk.
    pub fn shown(&self) -> Option<&Value> {
        self.eff.value()
    }

    pub fn editable(&self) -> bool {
        self.control != Control::ReadOnly
    }
}

/// Select mode moves and commands; Edit mode types. Keeping them apart is what
/// frees the single-letter keys -- `w` can mean write because in Select mode
/// nothing is listening for the letter w.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Select,
    Edit,
}

/// The four case transforms the `c` menu offers, in menu order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    Capitalize,
    Title,
    Lower,
    Upper,
}

/// The order `~` steps them in, which is the order the `f` menu lists them:
/// the two that keep words apart first, then the two that flatten everything.
const CASE_RING: [Case; 4] = [Case::Capitalize, Case::Title, Case::Lower, Case::Upper];

impl Case {
    pub fn name(self) -> &'static str {
        match self {
            Case::Capitalize => "capitalize",
            Case::Title => "title case",
            Case::Lower => "lower case",
            Case::Upper => "upper case",
        }
    }

    /// Capitalize is sentence case -- one leading capital, the rest lowered --
    /// and Title capitalizes every word. Both lower first, so a SHOUTED value
    /// comes back readable instead of staying shouted.
    pub fn apply(self, s: &str) -> String {
        match self {
            Case::Lower => s.to_lowercase(),
            Case::Upper => s.to_uppercase(),
            Case::Capitalize => upper_first(&s.to_lowercase()),
            Case::Title => {
                let lower = s.to_lowercase();
                let words: Vec<&str> = lower.split_inclusive(char::is_whitespace).collect();
                let last = words.iter().rposition(|w| !bare(w).is_empty()).unwrap_or(0);
                let mut out = String::with_capacity(lower.len());
                // The first word of the title, and the first after a colon --
                // both open a phrase, and a phrase never opens lowered.
                let mut opens = true;
                for (i, word) in words.iter().enumerate() {
                    if bare(word).is_empty() {
                        out.push_str(word);
                        continue;
                    }
                    if opens || i == last || !MINOR_WORDS.contains(&bare(word)) {
                        out.push_str(&upper_first(word));
                    } else {
                        out.push_str(word);
                    }
                    opens = word.trim_end().ends_with(':');
                }
                out
            }
        }
    }
}

/// The words title case leaves lowered when they land inside a title:
/// articles, the coordinating conjunctions, and the short prepositions. The
/// first word and the last are capitalized whatever they are, so "The Long
/// Way" and "Something To Aim For" both survive.
///
/// Verbs are not here however short they are -- a lowered `is` reads as a
/// typo, which is the reason the list is a list and not a length rule.
const MINOR_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "for", "from", "if", "in", "into", "nor", "of",
    "off", "on", "onto", "or", "over", "per", "so", "the", "to", "up", "upon", "via", "with",
    "yet",
];

/// A word with its punctuation and trailing space taken off, which is the form
/// `MINOR_WORDS` is written in: `"(and "` → `"and"`.
fn bare(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Uppercase the first alphabetic character and leave the rest alone, so
/// quotes and brackets do not swallow the capital: `"foo"` → `"Foo"`.
fn upper_first(s: &str) -> String {
    let mut done = false;
    s.chars()
        .map(|c| {
            if !done && c.is_alphabetic() {
                done = true;
                return c.to_uppercase().collect::<String>();
            }
            c.to_string()
        })
        .collect()
}

/// Which controls hold prose a case transform can mean something for.
fn is_textual(control: Control) -> bool {
    matches!(
        control,
        Control::Text | Control::TextArea | Control::List | Control::HashTags
    )
}

#[derive(Default)]
pub struct WriteResults {
    /// The verb for the title: `Wrote` or `Renamed`. The dialog is the same
    /// shape for either -- a batch that half-worked needs the same list.
    pub verb: &'static str,
    pub ok: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    /// Written, but the rename that was to follow did not happen, and why.
    /// Kept apart from `failed`: the tags landed, and the count of writes
    /// must say so.
    pub not_renamed: Vec<(PathBuf, String)>,
}

impl WriteResults {
    /// Whether there is anything here the status line cannot carry.
    pub fn has_problems(&self) -> bool {
        !self.failed.is_empty() || !self.not_renamed.is_empty()
    }
}

/// The answer most of a mixed set's files hold, by code. A tie goes to the
/// one earlier in the set's own order, so the same selection always settles
/// the same way; values the set does not know rank after the ones it does.
/// None when no file holds anything.
fn majority(values: &[Option<Value>], opts: &[Opt]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for v in values {
        let Some(Value::Text(code)) = v else { continue };
        if code.trim().is_empty() {
            continue;
        }
        match counts.iter_mut().find(|(c, _)| c == code) {
            Some((_, n)) => *n += 1,
            None => counts.push((code.clone(), 1)),
        }
    }
    let rank = |c: &str| opts.iter().position(|o| o.code == c).unwrap_or(usize::MAX);
    counts
        .into_iter()
        .min_by(|(a, na), (b, nb)| nb.cmp(na).then_with(|| rank(a).cmp(&rank(b))))
        .map(|(c, _)| c)
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Where the running write has got to. One file at a time, so this is a
/// position in the batch plus a position within the file.
pub struct WriteProgress {
    /// 0-based index of the file being written.
    pub file: usize,
    pub total: usize,
    pub label: &'static str,
    /// Fraction of this file's work, 0..1.
    pub frac: f64,
}

impl WriteProgress {
    /// Across the whole batch, so the bar moves steadily through forty files
    /// rather than resetting on each.
    pub fn overall(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        ((self.file as f64 + self.frac.clamp(0.0, 1.0)) / self.total as f64).clamp(0.0, 1.0)
    }
}

/// One file's turn in the write queue: its plan, and the XMP it held when
/// the plan was built, which the writer checks its result against.
struct Job {
    file: usize,
    plan: FilePlan,
    xmp: BTreeMap<String, Value>,
    /// Rename the file from its tags once they are written. `r` on a file
    /// with edits pending flags this rather than renaming from the values
    /// the write is about to replace.
    rename: bool,
}

/// The write queue, shared with the writer thread (DESIGN §9).
///
/// A queue rather than a batch handed over whole, because the form stays
/// live while it drains. An edit made to a file still waiting its turn
/// replaces that file's job, so the file is written once, with everything,
/// instead of needing a second `w`. Only the file under the writer is out of
/// reach: its edit stays staged and is queued again by the next `w`.
#[derive(Default)]
pub struct WriteQueue {
    waiting: VecDeque<Job>,
    /// The file being written now.
    busy: Option<usize>,
    /// Files finished this run -- the progress bar's numerator.
    done: usize,
    /// A writer thread is alive and will take whatever is pushed. Read and
    /// set under the same lock the thread exits under, so a job pushed as it
    /// is deciding to stop is never orphaned.
    running: bool,
}

/// One line of the write-queue panel (DESIGN §7): a file waiting its turn,
/// or the one the writer has now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct QueueRow {
    /// Index into `files`, so the panel can mark the focused row.
    pub file: usize,
    pub name: String,
    /// Under the writer right now -- the row that carries the live bar.
    pub busy: bool,
}

/// Where a file stands in the write queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueuePlace {
    /// Under the writer right now.
    Busy,
    /// Waiting, with this many files to be written before it.
    Waiting(usize),
}

fn lock(q: &Mutex<WriteQueue>) -> std::sync::MutexGuard<'_, WriteQueue> {
    q.lock().unwrap_or_else(|e| e.into_inner())
}

pub enum Msg {
    Thumb(usize, Box<image::DynamicImage>),
    Media(usize, MediaInfo),
    /// A stage of the running write, from the writer thread.
    Progress(Box<WriteProgress>),
    /// One file is done: its index, a fresh probe of it if the write landed,
    /// and the outcome. Sent per file so the form catches up with each one as
    /// it finishes rather than at the end of a forty-file run.
    /// The last field is the rename that followed a flagged write, if one
    /// was asked for.
    WroteFile(usize, Option<Box<FileTags>>, Result<(), String>, Option<Result<Outcome, String>>),
    /// The queue ran dry: the whole run's outcome.
    Wrote(Box<WriteResults>),
    /// One outcome per file a `rename-video` run was given, by file index.
    Renamed(Vec<(usize, Result<Outcome, String>)>),
    /// Whether `r` would collide on one file: its index, the path that was
    /// asked about -- so an answer for a name the file has since left is
    /// dropped -- and the file already holding its target, if one does.
    Conflict(usize, PathBuf, Option<PathBuf>),
    /// One result per file a `yt-dlp` fetch was given, by file index: the
    /// field values its URL yielded, or why it yielded none.
    Fetched(Vec<(usize, Result<Vec<(&'static str, Value)>, String>)>),
    /// The place lookup's answer (§5.5): its hits, or why there are none, and
    /// whether the lookup was a typed place -- whose venue name is worth
    /// staging -- or a camera coordinate, whose nearest venue is not.
    Located(Result<Vec<Hit>, String>, bool),
}

/// Where an import reads from. The menu keeps a cursor on one of these, so
/// the choice can be made by moving rather than by knowing a letter -- `u`
/// and `f` still name one outright.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportSource {
    Url,
    Filename,
    /// A place, looked up: the location block from a typed name, or from
    /// the coordinates the camera stored.
    Location,
}

impl ImportSource {
    /// The sources in the order the band paints them, which is the order the
    /// cursor walks.
    pub const ALL: [ImportSource; 3] = [ImportSource::Url, ImportSource::Filename, ImportSource::Location];
}

/// The place lookup, after `i l` (§5.5). Modal like the import menu it came
/// from, and painted in the same band: a place is typed, the helper runs off
/// the UI thread, and the hits are chosen from -- or, with exactly one, taken.
pub enum Locate {
    /// Typing what to look up. Opens holding what the location fields already
    /// say, so a lookup on a half-filled block is one ⏎.
    Ask(Editor),
    /// The helper is running. Esc here drops the answer when it comes.
    Looking,
    /// More than one hit: the cursor is on one of them.
    Pick { hits: Vec<Hit>, at: usize, named: bool },
}

/// The import menu's preview of one file: what each source has to offer.
pub struct ImportPreview {
    /// How many files the import would run over.
    pub files: usize,
    /// The URL the fetch would ask, if the file has one.
    pub url: Option<String>,
    /// The filename without its extension -- what the parser is given.
    pub stem: String,
    /// Fields the name carries that are empty on the file: label and value.
    pub fills: Vec<(String, Value)>,
    /// Fields the name carries that the file already holds, so are kept.
    pub keeps: Vec<String>,
    /// What the place lookup would start from: the location block as it
    /// stands, joined, if any of it is set.
    pub place: Option<String>,
    /// The coordinates on the file, if it has any -- what an empty lookup
    /// names.
    pub coords: Option<(f64, f64)>,
}

/// Staged edits, keyed by file index then by row key.
pub type Staged = BTreeMap<usize, BTreeMap<String, Value>>;

pub struct App {
    pub files: Vec<FileTags>,
    pub media: Vec<MediaInfo>,
    pub rows: Vec<Row>,
    /// Keys no field claims, kept as names so their aggregate can be recomputed
    /// for whichever files are in scope -- otherwise a custom key would still
    /// read ‹multiple› while looking at a single file.
    custom_keys: Vec<String>,
    pub focus: usize,
    /// None = aggregate view over every file; Some(i) = that one file.
    pub view: Option<usize>,
    pub inspector: bool,
    /// The import menu is up in the header band (§5.5, §9.4): a choice of
    /// where to seed the form from -- the page behind the URL field, or the
    /// filename -- with a preview of what each would bring. Modal until it is
    /// answered or dismissed, and painted where the inspector paints because
    /// both are answers about the file rather than the form.
    pub import_menu: bool,
    /// Which source the menu's cursor is on. Kept across openings so a second
    /// `i` offers the source the first one used, and set to whichever source
    /// has something to offer when the menu opens on a file with no URL.
    pub import_pick: ImportSource,
    /// The key-map overlay (§11). A screen of its own rather than a longer
    /// hint list: the badge bar has room for a mode's commands, not for the
    /// forty bindings the form actually has.
    pub help: bool,
    /// First line of the map on screen, so the overlay survives a terminal
    /// too short to hold it whole.
    pub help_scroll: u16,
    /// The furthest that scroll can usefully go, which only the painter knows
    /// -- it depends on the height of the box and on whether the map fell into
    /// one column or two. Set on every paint so `j` at the bottom stops rather
    /// than counting up invisibly and leaving `k` unresponsive.
    pub help_max: std::cell::Cell<u16>,
    pub status: String,
    /// The status line reports a failure. Painted in the error colour until
    /// the next key: a rename that did nothing looked exactly like one that
    /// worked, and the difference was a word in the middle of a grey line.
    pub status_error: bool,
    pub enums: Enums,
    /// Ride the faststart flag along on any remux we are already doing. On by
    /// default, per the brief.
    pub faststart: bool,
    /// The write plan, awaiting confirmation. Nothing reaches disk until this
    /// has been shown and accepted.
    pub pending: Option<Vec<FilePlan>>,
    /// The outcome of the last write, held until dismissed.
    pub results: Option<WriteResults>,
    /// A writer thread is draining the queue. Mirrors `queue.running` for
    /// the painter and the event loop's tick rate.
    pub writing: bool,
    /// The plans waiting to be written, shared with the writer thread.
    queue: Arc<Mutex<WriteQueue>>,
    /// Files to rename from their tags once their write lands. A rename is a
    /// field on the write, not a separate act: asked for while edits are
    /// pending, it waits for them.
    pub rename_after: BTreeSet<usize>,
    /// A `rename-video` run is in flight. It shells out to ffprobe and exiftool
    /// per file, so it runs off the UI thread like every other probe here --
    /// and while it does, the paths in `files` are the ones about to change,
    /// which is why `w` and a second `r` are held off until it lands.
    pub renaming: bool,
    /// Files whose rename would land on a name another file already holds,
    /// with that file's path. Asked of `rename-video` in the background on
    /// open and whenever a file's tags or name change, so the collision is
    /// on screen before `r` meets it. Nothing is ever renamed over it: the
    /// rename refuses a taken name whatever this says.
    pub conflicts: BTreeMap<usize, PathBuf>,
    /// A `yt-dlp` fetch is in flight (§5.5). Off the UI thread because a
    /// page extraction is seconds of network, and held to one at a time so
    /// two fetches cannot race each other onto the same field.
    pub fetching: bool,
    /// The place lookup in progress, if one is (§5.5). Owns every key while
    /// it is up, like the import menu it is reached from.
    pub locate: Option<Locate>,
    /// Live position of the running write. The write happens on its own thread
    /// precisely so this can be painted while it runs -- done inline, the event
    /// loop cannot redraw and a multi-gigabyte remux looks like a hang.
    pub progress: Option<WriteProgress>,
    /// The live control for the focused row. Recreated whenever focus moves, so
    /// there is no separate "edit mode": the focused field is always editable
    /// and typing goes straight into it, the way a GUI form behaves.
    pub editor: Option<Editor>,
    pub mode: Mode,
    /// Edits not yet written: file index → field key → value.
    ///
    /// Per file, not one map for the whole selection. Held globally an edit
    /// had no owner, so it followed the cursor onto the next file and was
    /// then dropped by the first file that already agreed with it --
    /// "equals what is on disk" was being read as "not an edit" against
    /// whichever file happened to be in view. Attributing each edit to the
    /// files it was made against is what makes `[` and `]` non-destructive.
    pub staged: Staged,
    undo: Vec<Staged>,
    redo: Vec<Staged>,
    pub quit: bool,
    /// Esc with staged edits asks once before discarding them.
    confirm_quit: bool,
    /// `f` in Select mode arms a one-shot format menu: the next key is a
    /// transform of the focused text rather than a command. A sub-menu rather
    /// than top-level letters because the letters worth using are already
    /// commands -- and because the menu has room to grow, which a handful of
    /// scattered top-level keys does not.
    pub format_pending: bool,
    /// The yank register. One slot, `y` fills it and `p` pastes it -- there is
    /// no need for named registers in a form of twenty fields.
    pub clipboard: Option<Value>,
    pub thumb_image: Option<image::DynamicImage>,
    pub thumb_for: Option<usize>,
    /// width/height of the current thumbnail, so the band can be shaped to the
    /// picture rather than the picture squeezed into a fixed band.
    pub thumb_aspect: Option<f32>,
    rx: Receiver<Msg>,
    tx: mpsc::Sender<Msg>,
}

impl App {
    pub fn new(files: Vec<FileTags>, custom: BTreeMap<String, Agg>, thumbnails: bool) -> Self {
        let custom_keys: Vec<String> = custom.keys().cloned().collect();
        let scope: Vec<usize> = (0..files.len()).collect();
        let rows = build_rows(&files, &scope, &Staged::new(), &custom_keys);
        let (tx, rx) = mpsc::channel();
        let n = files.len();
        let mut app = Self {
            media: vec![MediaInfo::default(); n],
            files,
            rows,
            custom_keys,
            focus: 0,
            view: None,
            inspector: false,
            import_menu: false,
            import_pick: ImportSource::Url,
            help: false,
            help_scroll: 0,
            help_max: std::cell::Cell::new(0),
            status: String::new(),
            status_error: false,
            enums: Enums::load(),
            faststart: true,
            pending: None,
            queue: Arc::default(),
            rename_after: BTreeSet::new(),
            results: None,
            writing: false,
            renaming: false,
            conflicts: BTreeMap::new(),
            fetching: false,
            locate: None,
            progress: None,
            editor: None,
            mode: Mode::Select,
            staged: Staged::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            quit: false,
            confirm_quit: false,
            format_pending: false,
            clipboard: None,
            thumb_image: None,
            thumb_for: None,
            thumb_aspect: None,
            rx,
            tx,
        };
        for i in 0..n {
            app.spawn_media(i);
        }
        if thumbnails {
            app.request_thumb(0);
        }
        app.open_editor();
        app
    }

    /// The file the header describes: the focused one in single view, else the
    /// first, so the band always has something to show.
    pub fn current_file(&self) -> usize {
        self.view.unwrap_or(0)
    }

    /// The files an edit made now applies to: one in single-file view, every
    /// file in the aggregate.
    pub fn scope(&self) -> Vec<usize> {
        match self.view {
            Some(i) => vec![i],
            None => (0..self.files.len()).collect(),
        }
    }

    /// How many distinct fields carry an edit, anywhere in the selection.
    /// Fields rather than file/field pairs, because "3 staged" should not
    /// become "12 staged" for the same three edits across four files.
    pub fn staged_count(&self) -> usize {
        let mut keys: Vec<&str> =
            self.staged.values().flat_map(|m| m.keys().map(String::as_str)).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.len()
    }

    fn spawn_media(&self, idx: usize) {
        let path = self.files[idx].path.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Ok(info) = thumb::probe_media(&path) {
                let _ = tx.send(Msg::Media(idx, info));
            }
        });
    }

    /// Extraction shells out to ffmpeg and can seek through a multi-gigabyte
    /// file, so it never runs on the UI thread.
    fn request_thumb(&mut self, idx: usize) {
        if self.thumb_for == Some(idx) || idx >= self.files.len() {
            return;
        }
        self.thumb_for = Some(idx);
        self.thumb_image = None;
        // The probe answers in milliseconds and the extract in seconds, so the
        // band is sized from the probe: the form must not jump down when the
        // picture of a portrait clip finally lands.
        self.thumb_aspect = self.media.get(idx).and_then(MediaInfo::aspect);
        let path = self.files[idx].path.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Ok(jpg) = thumb::extract(&path, 720, 720) {
                if let Ok(img) = image::ImageReader::open(&jpg).and_then(|r| Ok(r.decode())) {
                    if let Ok(img) = img {
                        let _ = tx.send(Msg::Thumb(idx, Box::new(img)));
                    }
                }
            }
        });
    }

    pub fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Thumb(i, img) => {
                    if self.thumb_for == Some(i) {
                        use image::GenericImageView;
                        let (w, h) = img.dimensions();
                        self.thumb_aspect =
                            (w > 0 && h > 0).then(|| w as f32 / h as f32);
                        self.thumb_image = Some(*img);
                    }
                }
                Msg::Media(i, info) => {
                    if self.thumb_for == Some(i) && self.thumb_aspect.is_none() {
                        self.thumb_aspect = info.aspect();
                    }
                    if i < self.media.len() {
                        self.media[i] = info;
                    }
                }
                Msg::Progress(p) => {
                    if self.writing {
                        self.progress = Some(*p);
                    }
                }
                Msg::WroteFile(i, fresh, res, renamed) => {
                    self.file_written(i, fresh.map(|b| *b), res, renamed)
                }
                Msg::Wrote(r) => self.finish_write(*r),
                Msg::Renamed(r) => self.finish_rename(r),
                Msg::Conflict(i, asked, taken) => {
                    if self.files.get(i).is_some_and(|f| f.path == asked) {
                        match taken {
                            Some(p) => self.conflicts.insert(i, p),
                            None => self.conflicts.remove(&i),
                        };
                    }
                }
                Msg::Fetched(r) => self.finish_fetch(r),
                Msg::Located(r, named) => self.finish_locate(r, named),
            }
        }
    }

    /// Union the focused list field across every file in scope.
    ///
    /// Setting a ‹multiple› list field otherwise means picking one file's
    /// values and destroying the rest, which is rarely what you want when
    /// tagging a batch -- you want everyone's actors, or every tag that appears
    /// anywhere. Order is first-seen; duplicates are folded case-insensitively
    /// so "Alice" and "alice" do not both survive.
    fn merge_focused(&mut self) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if !matches!(row.control, Control::List | Control::HashTags) {
            self.status = "merge applies to list fields".into();
            return;
        }
        let Agg::Mixed { values } = &row.eff else {
            self.status = "nothing to merge: the files already agree".into();
            return;
        };
        let merged = merge_values(values);
        if merged.is_empty() {
            self.status = "nothing to merge".into();
            return;
        }
        let key = row.key.clone();
        let n = merged.len();
        self.stage(key, Value::List(merged));
        self.status = format!("merged {n} value{} across the selection", if n == 1 { "" } else { "s" });
    }

    /// Every staged edit, for the confirmation dialog.
    ///
    /// Built from the staging map rather than from the visible rows: `w`
    /// writes every edit, including one made on a file that has since been
    /// walked away from, and the dialog is the last chance to see that.
    pub fn staged_summary(&self) -> Vec<StagedEdit> {
        let mut keys: Vec<&str> =
            self.staged.values().flat_map(|m| m.keys().map(String::as_str)).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter()
            .map(|key| {
                let edits: Vec<(usize, &Value)> = self
                    .staged
                    .iter()
                    .filter_map(|(i, m)| m.get(key).map(|v| (*i, v)))
                    .collect();
                let agreed = edits.windows(2).all(|w| w[0].1 == w[1].1);
                // How many distinct values this is about to flatten. One is a
                // change; several is a different act, and this is the last
                // place to notice it.
                let mut seen: Vec<Value> = Vec::new();
                for (i, _) in &edits {
                    if let Some(v) = disk_value(&self.files[*i], key) {
                        if !seen.contains(&v) {
                            seen.push(v);
                        }
                    }
                }
                StagedEdit {
                    label: key_label(key),
                    shown: match (agreed, edits.first()) {
                        (true, Some((_, v))) if v.is_empty() => "removed".into(),
                        (true, Some((_, Value::Text(t)))) => t.clone(),
                        (true, Some((_, Value::List(l)))) => l.join(", "),
                        _ => "‹multiple›".into(),
                    },
                    files: edits.len(),
                    overwrites: seen.len(),
                    refused: edits.iter().find_map(|(_, v)| field_error(key, v)),
                }
            })
            .collect()
    }

    /// What one file is about to have changed, field by field, for its line
    /// in the write dialog. The list above it says what each field becomes;
    /// this says which of them land on which file -- in a batch they need not
    /// be the same.
    pub fn file_edits(&self, path: &std::path::Path) -> Vec<FileEdit> {
        let Some(i) = self.files.iter().position(|f| f.path == path) else { return Vec::new() };
        self.staged
            .get(&i)
            .map(|edits| {
                edits
                    .iter()
                    .map(|(key, v)| FileEdit {
                        label: key_label(key),
                        removed: v.is_empty(),
                        refused: field_error(key, v).is_some(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Build the plan for the files in scope and hold it for confirmation.
    fn prepare_write(&mut self) {
        self.commit_editor();
        // The paths a plan would be built on are the ones a rename is in the
        // middle of changing.
        if self.renaming {
            self.status = "rename in progress".into();
            return;
        }
        // The fetch is about to stage edits; a plan built now would miss them.
        if self.fetching {
            self.status = "fetch in progress".into();
            return;
        }
        if self.staged.is_empty() {
            self.status = "nothing to write".into();
            return;
        }
        // A field whose value cannot be stored is left out of the plan rather
        // than written wrong (§5.4). The rest of the form still goes, and the
        // refused edit stays staged -- `finish_write` drops an edit only once
        // disk agrees with it, and disk never will until it is fixed.
        let mut refused: BTreeSet<&str> = BTreeSet::new();
        let mut why: Option<String> = None;
        // Every staged edit, not just the ones in view: an edit belongs to the
        // file it was made on, and silently skipping the file you are not
        // looking at is how a batch loses half its work.
        let plans: Vec<FilePlan> = self
            .staged
            .iter()
            .map(|(i, edits)| {
                let sound: BTreeMap<String, Value> = edits
                    .iter()
                    .filter(|(k, v)| match field_error(k, v) {
                        Some(e) => {
                            refused.insert(field_by_id(k).map(|f| f.label).unwrap_or(k.as_str()));
                            why.get_or_insert(e);
                            false
                        }
                        None => true,
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                plan::build(&self.files[*i], &sound, self.faststart)
            })
            .filter(|p| !p.is_empty())
            .collect();
        // One sentence for the whole write: which fields were dropped, and the
        // first reason. Naming every reason would bury the field names, and
        // the field is what the user has to go and fix.
        let refused_note = why.map(|why| {
            format!("{} not written · {why}", refused.into_iter().collect::<Vec<_>>().join(", "))
        });
        if plans.is_empty() {
            self.status = refused_note.unwrap_or_else(|| "nothing to write".into());
            return;
        }
        self.status = refused_note.unwrap_or_default();
        self.pending = Some(plans);
    }

    /// Queue the confirmed plan and make sure a writer thread is draining it.
    ///
    /// Off-thread because a remux is minutes of work and the event loop must
    /// keep painting and, now, keep editing: the form stays live while the
    /// queue drains, so the next file's tags can be typed while this one is
    /// being remuxed. A second `w` while it runs appends to the queue; a file
    /// already waiting gets its plan replaced rather than a second turn.
    fn apply(&mut self) {
        let Some(plans) = self.pending.take() else { return };
        let jobs: Vec<Job> = plans
            .into_iter()
            .filter_map(|plan| {
                let file = self.files.iter().position(|f| f.path == plan.path)?;
                let rename = self.rename_after.contains(&file);
                Some(Job { file, xmp: self.files[file].xmp.clone(), plan, rename })
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let n = jobs.len();
        let spawn = {
            let mut q = lock(&self.queue);
            for job in jobs {
                match q.waiting.iter().position(|j| j.file == job.file) {
                    Some(at) => q.waiting[at] = job,
                    None => q.waiting.push_back(job),
                }
            }
            if q.running {
                false
            } else {
                q.running = true;
                q.done = 0;
                true
            }
        };
        self.writing = true;
        if spawn {
            self.spawn_writer();
        } else {
            self.status =
                format!("queued {n} file{} behind the running write", if n == 1 { "" } else { "s" });
        }
    }

    /// The writer thread: take the front of the queue, write it, report, and
    /// repeat until the queue is empty. Progress goes back through the same
    /// channel the thumbnails use.
    fn spawn_writer(&self) {
        let queue = Arc::clone(&self.queue);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut written: Vec<PathBuf> = Vec::new();
            let mut failed: Vec<(PathBuf, String)> = Vec::new();
            let mut not_renamed: Vec<(PathBuf, String)> = Vec::new();
            loop {
                let (job, file_no) = {
                    let mut q = lock(&queue);
                    match q.waiting.pop_front() {
                        Some(j) => {
                            q.busy = Some(j.file);
                            (j, q.done)
                        }
                        None => {
                            q.busy = None;
                            q.running = false;
                            break;
                        }
                    }
                };
                let mut on = |s: write::Step| {
                    // The denominator is re-read per tick: the queue may have
                    // grown since this file started.
                    let total = {
                        let q = lock(&queue);
                        q.done + 1 + q.waiting.len()
                    };
                    let _ = tx.send(Msg::Progress(Box::new(WriteProgress {
                        file: file_no,
                        total,
                        label: s.label,
                        frac: s.frac,
                    })));
                };
                let res = write::execute(&job.plan, &job.xmp, &mut on).map_err(|e| e.to_string());
                match &res {
                    Ok(()) => written.push(job.plan.path.clone()),
                    // A bad file in a batch must not cost the others their write.
                    Err(e) => failed.push((job.plan.path.clone(), e.clone())),
                }
                // The rename runs on the tags just written, which is the
                // whole reason it waited. Only after a write that landed: a
                // file whose write failed keeps the name its old tags gave it.
                let renamed = (job.rename && res.is_ok()).then(|| {
                    on(write::Step { label: "renaming", frac: 1.0 });
                    rename::run(&job.plan.path).map_err(|e| format!("{e:#}"))
                });
                let mut path = job.plan.path.clone();
                match &renamed {
                    Some(Ok(Outcome::Renamed(to))) => path = to.clone(),
                    Some(Ok(Outcome::Taken(to))) => {
                        not_renamed.push((path.clone(), format!("name taken by {}", file_name(to))))
                    }
                    Some(Err(e)) => not_renamed.push((path.clone(), e.clone())),
                    Some(Ok(Outcome::Unchanged)) | None => {}
                }
                // Probed here, off the UI thread, so the form does not stall
                // on ffprobe and exiftool between one file and the next.
                let fresh = res
                    .is_ok()
                    .then(|| crate::tags::probe::probe(&path).ok())
                    .flatten()
                    .map(Box::new);
                {
                    let mut q = lock(&queue);
                    q.done += 1;
                    q.busy = None;
                }
                let _ = tx.send(Msg::WroteFile(job.file, fresh, res, renamed));
            }
            let _ = tx.send(Msg::Wrote(Box::new(WriteResults {
                verb: "Wrote",
                ok: written,
                failed,
                not_renamed,
            })));
        });
    }

    /// One file is off the queue: show what is now on disk, and drop the
    /// edits it carries. An edit is dropped only once the file agrees with
    /// it, so a failed write keeps what was typed (see `finish_write`).
    ///
    /// The file's own job, if `w` queued one again while it was being
    /// written, is rebuilt from whatever is still staged -- a plan built
    /// before the write would carry the edits that just landed.
    fn file_written(
        &mut self,
        i: usize,
        fresh: Option<FileTags>,
        _res: Result<(), String>,
        renamed: Option<Result<Outcome, String>>,
    ) {
        if let (Some(slot), Some(fresh)) = (self.files.get_mut(i), fresh) {
            *slot = fresh;
        }
        // The rename ran, one way or another; the flag has done its job. A
        // refusal is reported when the run settles, not carried forward to
        // silently retry on the next write.
        if let Some(outcome) = renamed {
            self.rename_after.remove(&i);
            if let (Ok(Outcome::Renamed(to)), Some(f)) = (outcome, self.files.get_mut(i)) {
                f.path = to;
            }
        }
        self.drop_landed();
        self.sync_queue(&[i]);
        self.rebuild_rows();
        // New tags are a new name to want, and possibly a taken one.
        self.check_renames(vec![i]);
    }

    /// Forget every staged edit the files now carry.
    ///
    /// Compared against what is on disk rather than against a list of files
    /// that succeeded: in a mixed batch an edit can land on four files and
    /// fail on the fifth, and it is still an edit until the fifth has it.
    fn drop_landed(&mut self) {
        let files = &self.files;
        for (i, edits) in self.staged.iter_mut() {
            let Some(file) = files.get(*i) else { continue };
            edits.retain(|key, value| !landed(file, key, value));
        }
        self.staged.retain(|_, edits| !edits.is_empty());
        if self.staged.is_empty() {
            self.undo.clear();
            self.redo.clear();
        }
    }

    /// Load the queue with jobs that write nothing, so a test can paint the
    /// queue panel without starting a writer thread. The plans are empty:
    /// nothing put here by a test ever reaches `write::execute`.
    #[cfg(test)]
    pub fn fake_queue(&mut self, busy: Option<usize>, waiting: &[usize]) {
        let plan = |file: usize| FilePlan {
            path: self.files[file].path.clone(),
            writer: crate::tags::plan::Writer::Native,
            atoms: Vec::new(),
            xmp: Vec::new(),
            faststart: false,
            layout: crate::tags::atoms::Layout::FastStart,
            why: "test",
        };
        let jobs: Vec<Job> = waiting
            .iter()
            .map(|&file| Job { file, plan: plan(file), xmp: BTreeMap::new(), rename: false })
            .collect();
        let mut q = lock(&self.queue);
        q.busy = busy;
        q.waiting = jobs.into();
    }

    /// The queue as the header panel lists it: the file under the writer
    /// first, then the ones waiting in the order they will be taken, capped
    /// at `max` rows with the true length beside it. A panel six rows tall
    /// cannot show forty files, and a list that silently stops at six is a
    /// list that lies about how much is left -- so the count comes with it.
    pub fn queue_rows(&self, max: usize) -> (Vec<QueueRow>, usize) {
        let q = lock(&self.queue);
        let total = usize::from(q.busy.is_some()) + q.waiting.len();
        let rows = q
            .busy
            .map(|f| (f, true))
            .into_iter()
            .chain(q.waiting.iter().map(|j| (j.file, false)))
            .take(max)
            .map(|(file, busy)| QueueRow {
                file,
                name: self.files.get(file).map(|f| file_name(&f.path)).unwrap_or_default(),
                busy,
            })
            .collect();
        (rows, total)
    }

    /// How many files stand where, between an edit and the disk. Counted in
    /// files rather than fields: "8 staged" meaning eight *fields* stayed at
    /// eight until the last file of a batch landed, and read as stuck while
    /// the writes went through underneath it. A file is counted once, at the
    /// furthest point it has reached -- a queued file still carries its
    /// staged edits, but "queued" is the news.
    pub fn pending(&self) -> Pending {
        let q = lock(&self.queue);
        let mut out = Pending::default();
        for i in 0..self.files.len() {
            if q.busy == Some(i) {
                out.writing += 1;
            } else if q.waiting.iter().any(|j| j.file == i) {
                out.queued += 1;
            } else if self.staged.get(&i).is_some_and(|e| !e.is_empty()) {
                out.staged += 1;
            }
        }
        out.rename = self.rename_after.len();
        out
    }

    /// Where `file` stands in the write queue, if it is in it.
    pub fn queue_place(&self, file: usize) -> Option<QueuePlace> {
        let q = lock(&self.queue);
        if q.busy == Some(file) {
            return Some(QueuePlace::Busy);
        }
        let at = q.waiting.iter().position(|j| j.file == file)?;
        Some(QueuePlace::Waiting(at + usize::from(q.busy.is_some())))
    }

    /// Bring the queued jobs for `files` up to date with the staging map.
    ///
    /// This is what lets an edit to a queued file be saved with ⏎ alone: the
    /// file's job is rebuilt from every sound edit it now carries, or removed
    /// if nothing is left to write. A file under the writer is left alone --
    /// there is no taking a plan back from a remux in progress -- and the
    /// caller is told, so the status line can say the edit needs `w` again.
    /// Unsound edits are skipped the same way `prepare_write` skips them.
    fn sync_queue(&mut self, files: &[usize]) -> QueueSync {
        let mut out = QueueSync::default();
        let mut q = lock(&self.queue);
        if q.busy.is_none() && q.waiting.is_empty() {
            return out;
        }
        for &i in files {
            if q.busy == Some(i) {
                out.busy.push(i);
                continue;
            }
            let Some(at) = q.waiting.iter().position(|j| j.file == i) else { continue };
            let Some(file) = self.files.get(i) else { continue };
            let sound: BTreeMap<String, Value> = self
                .staged
                .get(&i)
                .map(|edits| {
                    edits
                        .iter()
                        .filter(|(k, v)| field_error(k, v).is_none())
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            let plan = plan::build(file, &sound, self.faststart);
            if plan.is_empty() {
                q.waiting.remove(at);
            } else {
                let rename = self.rename_after.contains(&i);
                q.waiting[at] = Job { file: i, xmp: file.xmp.clone(), plan, rename };
            }
            out.refreshed += 1;
        }
        out
    }

    /// Say what `sync_queue` did, where it did anything.
    fn note_queue(&mut self, sync: &QueueSync) {
        if let Some(&i) = sync.busy.first() {
            let name = self.files.get(i).map(|f| file_name(&f.path)).unwrap_or_default();
            self.status = format!("{name} is being written now · press w to queue this edit");
            return;
        }
        if sync.refreshed > 0 {
            self.status = match sync.refreshed {
                1 => "saved to the queued write".into(),
                n => format!("saved to the queued writes for {n} files"),
            };
        }
    }

    /// The queue ran dry: settle up.
    ///
    /// The files were re-read one by one as they finished; what is left is
    /// the count, and the dialog -- shown only when something failed, since a
    /// clean run should not interrupt the typing the queue exists to allow.
    /// An edit is dropped only once the files agree with it. Clearing the
    /// whole staging map here cost a failed write everything that had been
    /// typed into it -- the form was the only place those edits existed, and
    /// the retry the error message invites began with retyping them.
    fn finish_write(&mut self, results: WriteResults) {
        self.drop_landed();
        self.rebuild_rows();
        // A `w` between the queue running dry and this message arriving has
        // already started another thread; believe the queue, not the message.
        self.writing = lock(&self.queue).running;
        self.progress = None;
        let total = results.ok.len() + results.failed.len();
        let kept = self.staged_count();
        self.status = if kept == 0 {
            format!("wrote {} of {}", results.ok.len(), total)
        } else {
            format!(
                "wrote {} of {}; {} edit{} kept",
                results.ok.len(),
                total,
                kept,
                if kept == 1 { "" } else { "s" }
            )
        };
        if !results.not_renamed.is_empty() {
            self.status.push_str(&format!(" · {} not renamed", results.not_renamed.len()));
        }
        self.status_error = results.has_problems();
        // The results stay up until a key, with any unwritten edits still
        // staged. Only for a failure: the list of what went wrong is the
        // point of the dialog, and a clean run has nothing to list.
        if results.has_problems() {
            self.results = Some(results);
        }
    }

    /// `r`: hand the files in scope to `rename-video`, which names each one
    /// from its own tags. Filename sync as designed (DESIGN §9.4) is not built
    /// here; the tool already composes both of this library's grammars, and one
    /// grammar in one place is the point.
    ///
    /// Disk tags, not staged ones: the tool re-probes each file, so a rename
    /// run before the write would build the name out of the values the edit is
    /// about to replace. So a file with edits pending -- staged, or already on
    /// the write queue -- is *flagged* instead: the rename becomes part of its
    /// write and runs on the tags the write leaves. `r` again takes the flag
    /// off. Files with nothing pending are renamed now, as before.
    fn rename_files(&mut self) {
        self.commit_editor();
        if self.renaming {
            return;
        }
        let scope = self.scope();
        let (pending, clean): (Vec<usize>, Vec<usize>) = scope.into_iter().partition(|i| {
            self.staged.get(i).is_some_and(|e| !e.is_empty()) || self.queue_place(*i).is_some()
        });
        let mut note = String::new();
        if !pending.is_empty() {
            // One key toggles the whole selection the same way: on if any of
            // it was off, otherwise off.
            let arm = pending.iter().any(|i| !self.rename_after.contains(i));
            for i in &pending {
                if arm {
                    self.rename_after.insert(*i);
                } else {
                    self.rename_after.remove(i);
                }
            }
            self.sync_queue(&pending);
            let n = pending.len();
            note = match (arm, n) {
                (true, 1) => "rename queued · runs after the write".into(),
                (true, n) => format!("rename queued for {n} files · runs after the write"),
                (false, 1) => "rename unqueued".into(),
                (false, n) => format!("rename unqueued for {n} files"),
            };
        }
        let jobs: Vec<(usize, PathBuf)> =
            clean.iter().map(|i| (*i, self.files[*i].path.clone())).collect();
        let Some((_, first)) = jobs.first() else {
            self.status = note;
            return;
        };
        self.status = match jobs.len() {
            1 => format!("renaming {}", file_name(first)),
            n => format!("renaming {n} files"),
        };
        if !note.is_empty() {
            self.status = format!("{} · {note}", self.status);
        }
        self.renaming = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = jobs
                .iter()
                // A file the tool refuses must not cost the rest of the batch
                // its rename, so each carries its own outcome home.
                .map(|(i, p)| (*i, rename::run(p).map_err(|e| format!("{e:#}"))))
                .collect();
            let _ = tx.send(Msg::Renamed(out));
        });
    }

    /// Take the new paths and nothing else. `rename-video` writes no tags, so
    /// the model is still true of every file -- only where it lives changed,
    /// and re-probing to learn that would be minutes of ffprobe for a string.
    fn finish_rename(&mut self, out: Vec<(usize, Result<Outcome, String>)>) {
        self.renaming = false;
        let total = out.len();
        let mut renamed = 0usize;
        let mut name = String::new();
        let mut unchanged = 0usize;
        // A file that kept its name for a reason, with the reason. These go
        // to the results dialog rather than the status line: the reason is a
        // sentence about a 300-byte path, and the line has room for neither.
        let mut failed: Vec<(PathBuf, String)> = Vec::new();
        let mut ok: Vec<PathBuf> = Vec::new();
        for (i, r) in out {
            let path = self.files.get(i).map(|f| f.path.clone()).unwrap_or_default();
            match r {
                Ok(Outcome::Renamed(to)) => {
                    name = file_name(&to);
                    ok.push(to.clone());
                    if let Some(f) = self.files.get_mut(i) {
                        f.path = to;
                    }
                    renamed += 1;
                }
                Ok(Outcome::Unchanged) => unchanged += 1,
                Ok(Outcome::Taken(to)) => {
                    failed.push((path, format!("name taken by {}", file_name(&to))));
                    self.conflicts.insert(i, to);
                }
                Err(e) => failed.push((path, e)),
            }
        }
        // A name one file left may be the name another was blocked on, so
        // the whole selection is asked again rather than only the files run.
        self.check_renames((0..self.files.len()).collect());
        self.status_error = !failed.is_empty();
        self.status = match (renamed, total) {
            (0, 1) if unchanged == 1 => "already named from its tags".into(),
            (0, _) if failed.is_empty() => "already named from their tags".into(),
            (0, 1) => format!("not renamed: {}", failed[0].1),
            (0, t) => format!("renamed 0 of {t}"),
            (1, 1) => format!("renamed to {name}"),
            (n, t) if n == t => format!("renamed {n} files"),
            (n, t) => format!("renamed {n} of {t}"),
        };
        // Only a failure earns the dialog. A clean batch, or a file already
        // named right, is a one-line fact; a refusal is a paragraph.
        if !failed.is_empty() {
            self.results = Some(WriteResults { verb: "Renamed", ok, failed, not_renamed: vec![] });
        }
    }

    /// Ask `rename-video` where each of `files` wants to live, off the UI
    /// thread and one file at a time, and record the ones whose name is
    /// already held by another file. A missing tool, or a file it declines,
    /// is not a conflict -- it is a rename that cannot run, which `r` says
    /// when asked.
    pub fn check_renames(&self, files: Vec<usize>) {
        let jobs: Vec<(usize, PathBuf)> = files
            .into_iter()
            .filter_map(|i| self.files.get(i).map(|f| (i, f.path.clone())))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for (i, path) in jobs {
                let taken = rename::conflict(&path).ok().flatten();
                if tx.send(Msg::Conflict(i, path, taken)).is_err() {
                    return;
                }
            }
        });
    }

    /// What is wrong with one file, as the sentences its page raises: a
    /// rename that would land on another file, and staged values the write
    /// will leave out. Empty for a file with nothing to warn about. The file
    /// list colours a file by whether this is empty, so the two cannot
    /// disagree about which files need looking at.
    pub fn file_alerts(&self, i: usize) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(to) = self.conflicts.get(&i) {
            // The reason before the name: a composed name runs past the
            // width of the band, and what is cut off is the name.
            out.push(format!("rename blocked: another file is already named {}", file_name(to)));
        }
        if let Some(edits) = self.staged.get(&i) {
            for (key, value) in edits {
                if let Some(why) = field_error(key, value) {
                    out.push(format!("{}: {why}", key_label(key)));
                }
            }
        }
        out
    }

    /// On open: read the fields a structured filename carries into the fields
    /// the file leaves empty, the same import `i f` runs, staged and undoable
    /// in one step. Only a name with structure -- people, a channel, tags,
    /// stars, a date -- is read. A bare stem is all title to the parser, and
    /// `IMG_0412` or `clip-3` is not a title anyone chose.
    pub fn seed_from_filenames(&mut self) {
        let out: Vec<(usize, Result<Vec<(&'static str, Value)>, String>)> = (0..self.files.len())
            .map(|i| (i, filename::parse_path(&self.files[i].path)))
            .filter(|(_, fields)| fields.iter().any(|(id, _)| *id != "title"))
            .map(|(i, fields)| (i, Ok(fields)))
            .collect();
        if out.is_empty() {
            return;
        }
        let before = self.staged.clone();
        self.stage_import("filled", "", out, true);
        // A name that told the form nothing new says nothing on open: the
        // status line is for what happened, and nothing did.
        if before == self.staged {
            self.status.clear();
            self.status_error = false;
        } else {
            let from = if self.files.len() == 1 { "the filename" } else { "the filenames" };
            self.status = format!("{} from {from} · u undoes", self.status);
        }
        self.open_editor();
    }

    /// `i` then `u`: ask yt-dlp what the page behind the URL field says and
    /// stage it onto the other fields (§5.5). Per file in scope, each from
    /// its own URL, so a batch of downloads seeds itself in one key. The URL
    /// used is the one shown -- a URL just typed and not yet written counts,
    /// which is the common case: paste the page, press `i u`, get the form
    /// filled. It does not matter which field is focused: the source is the
    /// URL field, not the cursor.
    fn fetch_tags(&mut self) {
        self.commit_editor();
        if self.fetching {
            return;
        }
        let jobs: Vec<(usize, String)> = self
            .scope()
            .into_iter()
            .filter_map(|i| {
                let disk = disk_value(&self.files[i], "url");
                let url = overlay(disk, self.staged.get(&i).and_then(|m| m.get("url")))?;
                match url {
                    Value::Text(s) if !s.trim().is_empty() => Some((i, s.trim().to_string())),
                    _ => None,
                }
            })
            .collect();
        let Some((first, _)) = jobs.first() else {
            self.status = "no URL to fetch from".into();
            self.status_error = true;
            return;
        };
        self.status = match jobs.len() {
            1 => format!("fetching tags for {}", file_name(&self.files[*first].path)),
            n => format!("fetching tags for {n} files"),
        };
        self.fetching = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = jobs
                .iter()
                // One page down must not cost the rest of the batch its tags.
                .map(|(i, url)| (*i, fetch::fetch(url).map_err(|e| format!("{e:#}"))))
                .collect();
            let _ = tx.send(Msg::Fetched(out));
        });
    }

    /// Stage what the pages said, as one undoable step. A field a page did not
    /// answer is not in its list and so is not touched -- the existing value,
    /// staged or on disk, stays. A field it did answer is replaced: the point
    /// of pressing `i u` is to take the page's word for it, and `u` takes it
    /// back in one key if the page was wrong.
    fn finish_fetch(&mut self, out: Vec<(usize, Result<Vec<(&'static str, Value)>, String>)>) {
        self.fetching = false;
        self.stage_import("fetched", "the page agrees with the file", out, false);
    }

    /// `i` then `f`: read the fields the filename carries (§9.4) and stage
    /// them into the fields that are still empty. Only the empty ones, unlike
    /// the fetch: a page is an authority worth taking the word of, but a name
    /// was composed *from* tags, so where the container disagrees with it the
    /// container is the newer of the two. Per file in scope, each from its
    /// own name. Synchronous -- there is nothing to wait on.
    fn import_filename(&mut self) {
        self.commit_editor();
        let out = self
            .scope()
            .into_iter()
            .map(|i| {
                let fields = filename::parse_path(&self.files[i].path);
                let r = if fields.is_empty() {
                    Err(format!("nothing recognised in {}", file_name(&self.files[i].path)))
                } else {
                    Ok(fields)
                };
                (i, r)
            })
            .collect();
        self.stage_import("imported", "every field the name carries is already set", out, true);
    }

    /// Stage what a source said about each file, as one undoable step, and
    /// say what happened. A field a source did not answer is not in its list
    /// and so is not touched. With `only_empty`, a field that already shows
    /// a value keeps it -- the filename rule; without, the source's value
    /// replaces it -- the fetch's rule, where `u` takes the whole step back.
    fn stage_import(
        &mut self,
        verb: &str,
        agrees: &str,
        out: Vec<(usize, Result<Vec<(&'static str, Value)>, String>)>,
        only_empty: bool,
    ) {
        let total = out.len();
        let before = self.staged.clone();
        let mut filled = 0usize;
        let mut files = 0usize;
        let mut note = String::new();
        for (i, r) in out {
            match r {
                Ok(fields) => {
                    files += 1;
                    for (id, value) in fields {
                        filled += self.place(&[i], id, &value, only_empty);
                    }
                }
                Err(e) => note = e,
            }
        }
        if before != self.staged {
            self.undo.push(before);
            self.redo.clear();
            let touched: Vec<usize> = self.staged.keys().copied().collect();
            self.sync_queue(&touched);
        }
        self.rebuild_rows();
        let n_fields = |n: usize| format!("{n} field{}", if n == 1 { "" } else { "s" });
        // A page that would not answer is the one outcome here worth a colour.
        // The message already said so, in the middle of a grey line that looks
        // exactly like the line a successful import leaves -- which is how a
        // failed fetch got read as "nothing new" more than once.
        self.status_error = files < total;
        self.status = match (files, total) {
            (0, _) => note,
            (1, 1) if filled == 0 => format!("nothing new: {agrees}"),
            (1, 1) => format!("{verb} {}", n_fields(filled)),
            (n, t) if n == t => format!("{verb} {} across {n} files", n_fields(filled)),
            (n, t) => format!("{verb} {} across {n} of {t} files: {note}", n_fields(filled)),
        };
    }

    /// What the import menu shows beside each source, for the file in view
    /// (the first in scope, in the aggregate). Computed here rather than in
    /// the painter so the preview and the import cannot disagree about what
    /// the name says.
    pub fn import_preview(&self) -> ImportPreview {
        let idx = self.current_file();
        let scope = self.scope();
        let url = self.files.get(idx).and_then(|f| {
            let disk = disk_value(f, "url");
            match overlay(disk, self.staged.get(&idx).and_then(|m| m.get("url")))? {
                Value::Text(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                _ => None,
            }
        });
        let path = self.files.get(idx).map(|f| f.path.clone()).unwrap_or_default();
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mut fills = Vec::new();
        let mut keeps = Vec::new();
        for (id, value) in filename::parse_path(&path) {
            let label = key_label(id);
            let disk = self.files.get(idx).and_then(|f| disk_value(f, id));
            let now = overlay(disk, self.staged.get(&idx).and_then(|m| m.get(id)));
            if now.is_some_and(|v| !v.is_empty()) {
                keeps.push(label);
            } else {
                fills.push((label, value));
            }
        }
        let place = self.place_text(idx);
        let place = if place.is_empty() { None } else { Some(place) };
        ImportPreview { files: scope.len(), url, stem, fills, keeps, place, coords: self.coords_of(idx) }
    }

    /// The location block as the form shows it for one file, joined into the
    /// query a lookup would start from: "Coro Hotel, Makati, Metro Manila".
    fn place_text(&self, idx: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        for id in ["location_place", "location", "location_state", "location_country"] {
            let disk = self.files.get(idx).and_then(|f| disk_value(f, id));
            if let Some(Value::Text(s)) = overlay(disk, self.staged.get(&idx).and_then(|m| m.get(id))) {
                let s = s.trim();
                if !s.is_empty() && !parts.iter().any(|p| p == s) {
                    parts.push(s.to_string());
                }
            }
        }
        parts.join(", ")
    }

    /// The coordinates the form shows for one file, if they parse.
    fn coords_of(&self, idx: usize) -> Option<(f64, f64)> {
        let disk = self.files.get(idx).and_then(|f| disk_value(f, "coordinates"));
        match overlay(disk, self.staged.get(&idx).and_then(|m| m.get("coordinates")))? {
            Value::Text(s) => geocode::parse_iso6709(&s),
            _ => None,
        }
    }

    /// `i l`: open the lookup prompt over the location block as it stands.
    fn open_locate(&mut self) {
        self.commit_editor();
        let idx = self.current_file();
        let seed = Value::text(self.place_text(idx));
        let editor = Editor::new(crate::model::schema::Control::Text, Some(&seed), Vec::new());
        self.locate = Some(Locate::Ask(editor));
        self.status.clear();
    }

    /// ⏎ on the prompt. A typed place is searched for; an empty prompt on a
    /// file with coordinates names the place the camera recorded; an empty
    /// prompt with nothing to go on stays open and says so.
    fn run_locate(&mut self) {
        let Some(Locate::Ask(ed)) = &self.locate else { return };
        let query = match ed.value() {
            Value::Text(s) => s.trim().to_string(),
            _ => String::new(),
        };
        if !query.is_empty() {
            self.start_lookup(query);
        } else if !self.name_the_coordinates() {
            self.status = "type a place to look up; this file has no coordinates to name".into();
            self.status_error = true;
        }
    }

    /// Reverse lookup of the coordinates in view, from the Coordinates row
    /// or an empty prompt. False when the file has none to name.
    fn name_the_coordinates(&mut self) -> bool {
        let Some((lat, lon)) = self.coords_of(self.current_file()) else { return false };
        self.status = format!("naming the place at {}", geocode::iso6709(lat, lon));
        self.status_error = false;
        self.spawn_lookup(move || geocode::reverse(lat, lon), false);
        true
    }

    /// Search for a typed place, from the Place row or the `i l` prompt.
    fn start_lookup(&mut self, query: String) {
        self.status = format!("looking up {query}");
        self.status_error = false;
        self.spawn_lookup(move || geocode::search(&query), true);
    }

    fn spawn_lookup(&mut self, job: impl FnOnce() -> anyhow::Result<Vec<Hit>> + Send + 'static, named: bool) {
        self.locate = Some(Locate::Looking);
        // The suite must not reach MapKit: a test that commits a place checks
        // the state and answers the lookup itself.
        if cfg!(test) {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Located(job().map_err(|e| format!("{e:#}")), named));
        });
    }

    /// The helper answered. One hit is taken as read -- a single confirmation
    /// key on "Coro Hotel, Makati" would be a toll, and `u` takes it back --
    /// and several are offered to choose from. An answer to a lookup that was
    /// cancelled meanwhile is dropped.
    fn finish_locate(&mut self, r: Result<Vec<Hit>, String>, named: bool) {
        if !matches!(self.locate, Some(Locate::Looking)) {
            return;
        }
        match r {
            Err(e) => {
                self.locate = None;
                self.status = e;
                self.status_error = true;
            }
            Ok(mut hits) if hits.len() == 1 => self.stage_hit(hits.remove(0), named),
            Ok(hits) => self.locate = Some(Locate::Pick { hits, at: 0, named }),
        }
    }

    /// Walk the picker, wrapping like the form's own j/k.
    fn move_pick(&mut self, delta: isize) {
        if let Some(Locate::Pick { hits, at, .. }) = &mut self.locate {
            let n = hits.len() as isize;
            *at = (((*at as isize + delta) % n + n) % n) as usize;
        }
    }

    /// Stage one hit's location block onto every file in scope, as one
    /// undoable step, the way a fetch stages a page's answers.
    fn stage_hit(&mut self, hit: Hit, named: bool) {
        self.locate = None;
        let fields = hit.fields(named);
        let out = self.scope().into_iter().map(|i| (i, Ok(fields.clone()))).collect();
        self.stage_import("located", "the place agrees with the file", out, false);
        if !self.status_error {
            self.status = format!("{}: {}", hit.summary(), self.status);
        }
    }

    /// `i`: open the menu, with the cursor on a source that has something to
    /// offer. A file with no URL cannot be fetched for, so opening on `url`
    /// there would put ⏎ on the one choice that refuses -- the cursor starts
    /// on the filename instead, and `u` still reaches the other one.
    fn open_import(&mut self) {
        if self.import_preview().url.is_none() {
            self.import_pick = ImportSource::Filename;
        }
        self.import_menu = true;
        self.status.clear();
    }

    /// Walk the cursor, wrapping the way the form's own j/k do.
    fn move_import(&mut self, delta: isize) {
        let all = ImportSource::ALL;
        let n = all.len() as isize;
        let at = all.iter().position(|s| *s == self.import_pick).unwrap_or(0) as isize;
        self.import_pick = all[(((at + delta) % n + n) % n) as usize];
    }

    /// Close the menu and run the chosen source.
    fn run_import(&mut self, source: ImportSource) {
        self.import_menu = false;
        self.import_pick = source;
        match source {
            ImportSource::Url => self.fetch_tags(),
            ImportSource::Filename => self.import_filename(),
            ImportSource::Location => self.open_locate(),
        }
    }

    /// Route by mode. Select moves and commands; Edit types.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.results.is_some() {
            self.results = None;
            return;
        }
        // Any key after a failure is the acknowledgement; the message stays,
        // the colour goes.
        self.status_error = false;
        // The key map owns every key while it is up, the same way a dialog
        // does -- otherwise reading it would edit the form behind it. Only
        // scrolling stays live; anything else closes it.
        if self.help {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = (self.help_scroll + 1).min(self.help_max.get())
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                _ => {
                    self.help = false;
                    self.help_scroll = 0;
                    self.status.clear();
                }
            }
            return;
        }
        // A dialog owns every key while it is up: a stray character must not
        // leak into a form field behind a prompt asking to write.
        if self.pending.is_some() {
            match key.code {
                KeyCode::Enter => self.apply(),
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.pending = None;
                    self.status = "write cancelled".into();
                }
                _ => {}
            }
            return;
        }
        // Save is the one command that works from either mode: ⌘S where the
        // terminal reports it (kitty protocol) and ⌃S everywhere. It commits
        // whatever is half-typed first, so what is written is what is on
        // screen, and it leaves the form in Select mode behind the dialog.
        if key.code == KeyCode::Char('s')
            && key.modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::CONTROL)
        {
            self.prepare_write();
            self.mode = Mode::Select;
            return;
        }
        match self.mode {
            Mode::Edit => self.edit_key(key),
            Mode::Select => self.select_key(key),
        }
    }

    fn edit_key(&mut self, key: KeyEvent) {
        // The control sees every key first, and whatever it hands back is a
        // command. Matching Enter here before offering it to the control was
        // what stopped an enum menu from ever applying its highlight: the app
        // committed the field while the menu was still holding the choice.
        if let Some(ed) = &mut self.editor {
            if ed.handle(key) == Reaction::Consumed {
                self.status.clear();
                return;
            }
        }
        match key.code {
            // Commit and stop editing.
            // Cleared first: the commit may have something to say about the
            // queue, and that has to outlive the keystroke.
            KeyCode::Enter => {
                self.status.clear();
                self.commit_editor();
                self.mode = Mode::Select;
            }
            // Commit and carry straight on to the next field, which is what
            // tab means in every form.
            KeyCode::Tab => self.move_focus(1),
            KeyCode::BackTab => self.move_focus(-1),
            // Vertical movement leaves the field the way it found it: saved,
            // and back in Select mode. Only a control that does not want the
            // letters gets here, so `j`/`k` still type into a text field.
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_focus(1);
                self.mode = Mode::Select;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_focus(-1);
                self.mode = Mode::Select;
                self.status.clear();
            }
            // Abandon this field's edit. Reseeding restores whatever the row
            // showed before -- the staged value if there was one, else disk.
            KeyCode::Esc => {
                self.open_editor();
                self.mode = Mode::Select;
                self.status = "edit cancelled".into();
            }
            _ => {}
        }
    }

    fn select_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code != KeyCode::Esc && key.code != KeyCode::Char('q') {
            self.confirm_quit = false;
        }
        // The format menu owns the next key entirely: `t` there means title
        // case, not theme, and an unknown key cancels rather than falling
        // through to a command the user did not mean to reach.
        if self.format_pending {
            self.format_pending = false;
            match key.code {
                KeyCode::Char('c') if !ctrl => self.apply_case(Case::Capitalize),
                KeyCode::Char('t') if !ctrl => self.apply_case(Case::Title),
                KeyCode::Char('l') if !ctrl => self.apply_case(Case::Lower),
                KeyCode::Char('u') if !ctrl => self.apply_case(Case::Upper),
                _ => self.status = "format cancelled".into(),
            }
            return;
        }
        // The import menu owns every key while it is up, but unlike the format
        // menu it is a selector rather than a single keystroke: j/k walk the
        // sources with the preview redrawn under the cursor, ⏎ runs the one
        // the cursor is on. `u` and `f` still name a source outright -- the
        // menu is somewhere to look before choosing, not a toll on already
        // knowing. A key that means nothing here is swallowed rather than
        // treated as a cancel: in a menu you move around in, an unrecognised
        // key is a misfire, and closing on it would throw away the preview
        // the user is still reading.
        // The lookup owns every key while it is up, in three shapes: a line
        // being typed, a wait, and a list being chosen from. Esc leaves each
        // of them, and leaving the wait drops the answer when it arrives.
        if let Some(locate) = &mut self.locate {
            match locate {
                Locate::Ask(ed) => match key.code {
                    KeyCode::Esc => {
                        self.locate = None;
                        self.status = "lookup cancelled".into();
                    }
                    KeyCode::Enter => self.run_locate(),
                    _ => {
                        ed.handle(key);
                    }
                },
                Locate::Looking => {
                    if key.code == KeyCode::Esc {
                        self.locate = None;
                        self.status = "lookup cancelled".into();
                    }
                }
                Locate::Pick { hits, at, named } => match key.code {
                    KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => self.move_pick(1),
                    KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => self.move_pick(-1),
                    KeyCode::Enter => {
                        let (hit, named) = (hits[*at].clone(), *named);
                        self.stage_hit(hit, named);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.locate = None;
                        self.status = "lookup cancelled".into();
                    }
                    _ => {}
                },
            }
            return;
        }
        if self.import_menu {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => self.move_import(1),
                KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => self.move_import(-1),
                KeyCode::Enter => self.run_import(self.import_pick),
                KeyCode::Char('u') if !ctrl => self.run_import(ImportSource::Url),
                KeyCode::Char('f') if !ctrl => self.run_import(ImportSource::Filename),
                KeyCode::Char('l') if !ctrl => self.run_import(ImportSource::Location),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => {
                    self.import_menu = false;
                    self.status = "import cancelled".into();
                }
                _ => {}
            }
            return;
        }
        // ⌘Z is undo as well as `u`, and ⌘⇧Z redo as well as ⌃R. The vi keys
        // are the ones worth learning, but the form is a form: the undo
        // gesture every other app on the machine trains should not be the one
        // thing here that does nothing. Like ⌘S it needs a terminal that
        // reports SUPER (the kitty keyboard protocol).
        if key.modifiers.contains(KeyModifiers::SUPER) {
            match key.code {
                KeyCode::Char('z') => {
                    self.undo();
                    return;
                }
                KeyCode::Char('Z') => {
                    self.redo();
                    return;
                }
                _ => {}
            }
        }
        match (key.code, ctrl) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) | (KeyCode::Tab, _) => {
                self.move_focus(1)
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
                self.move_focus(-1)
            }
            (KeyCode::Char('h'), false) | (KeyCode::Left, _) => self.nudge(-1),
            (KeyCode::Char('l'), false) | (KeyCode::Right, _) => self.nudge(1),
            (KeyCode::Char(c @ '0'..='9'), false) => self.type_digit(c),
            (KeyCode::Char('g'), false) => self.jump(0),
            (KeyCode::Char('G'), false) => self.jump(self.rows.len().saturating_sub(1)),
            (KeyCode::Enter, _) => self.begin_edit(),
            (KeyCode::Char('I'), false) => {
                self.inspector = !self.inspector;
                self.status.clear();
            }
            (KeyCode::Char('i'), false) => {
                self.commit_editor();
                self.open_import();
            }
            (KeyCode::Char('y') | KeyCode::Char('c'), false) => self.yank(),
            (KeyCode::Char('p'), false) => self.paste(),
            (KeyCode::Char(']'), false) | (KeyCode::Char('n'), true) => self.cycle_file(1),
            (KeyCode::Char('['), false) | (KeyCode::Char('p'), true) => self.cycle_file(-1),
            (KeyCode::Char('a'), false) => {
                self.commit_editor();
                self.view = None;
                self.rebuild_rows();
                // Which view this is, is the view line's to say.
                self.status.clear();
            }
            (KeyCode::Char('m'), false) => self.merge_focused(),
            (KeyCode::Char('o'), false) => self.open_file(),
            (KeyCode::Char('O'), false) => self.copy_out(false),
            (KeyCode::Char('b'), false) => self.copy_out(true),
            (KeyCode::Char('u'), false) => self.undo(),
            (KeyCode::Char('r'), true) => self.redo(),
            (KeyCode::Char('r'), false) => self.rename_files(),
            (KeyCode::Backspace, _) => self.clear_focused(),
            (KeyCode::Char('w'), false) => self.prepare_write(),
            (KeyCode::Char('t'), false) => {
                self.status = format!("theme: {}", theme::cycle());
            }
            (KeyCode::Char('f'), false) => self.begin_format(),
            (KeyCode::Char('~'), false) => self.cycle_case(),
            (KeyCode::Char('?'), false) => {
                self.commit_editor();
                self.help = true;
                self.help_scroll = 0;
                self.status.clear();
            }
            (KeyCode::Char('F'), false) => {
                self.faststart = !self.faststart;
                self.status = format!("faststart {}", if self.faststart { "on" } else { "off" });
            }
            (KeyCode::Esc, _) if self.revert_focused_set() => {}
            (KeyCode::Char('q'), false) | (KeyCode::Esc, _) => self.escape(),
            _ => {}
        }
    }

    fn jump(&mut self, to: usize) {
        self.commit_editor();
        self.focus = to.min(self.rows.len().saturating_sub(1));
        self.open_editor();
    }

    /// ⏎ opens a field. A fixed set is the exception: it has no open state at
    /// all any more, because h/l already step it in place and a mode that
    /// exists only to press h/l in is a mode nobody needs (§5.7). The keys are
    /// live wherever the row is focused, so ⏎ says so rather than doing
    /// nothing silently.
    ///
    /// A rating is a fixed set of six values and is treated as one: h/l nudge
    /// it, 0-5 name it outright, and j/k keep meaning "next field" instead of
    /// being swallowed by a mode whose only keys were the ones that already
    /// worked outside it.
    fn begin_edit(&mut self) {
        match self.rows.get(self.focus) {
            Some(row) if !row.editable() => {
                self.status = format!("{} is read-only", row.label);
            }
            Some(row) if row.control == Control::Enum => {
                self.status = format!("{} · h/l or ←→ to choose", row.label);
            }
            Some(row) if row.control == Control::Stars => {
                self.status = format!("{} · h/l or ←→ to nudge, 0-5 to set", row.label);
            }
            // Place is not typed into so much as asked: ⏎ on it is `i l`, the
            // lookup prompt over the block as it stands, and the hit fills
            // the row. The row itself still takes a value from a fetch, a
            // paste, or the prompt's own answer.
            Some(row) if row.key == "location_place" => self.open_locate(),
            // Coordinates the file already holds are named, not retyped: ⏎
            // runs the reverse lookup straight away. An empty row opens for
            // typing like any other.
            Some(row) if row.key == "coordinates" && self.coords_of(self.current_file()).is_some() => {
                self.commit_editor();
                self.name_the_coordinates();
            }
            // An empty Date opens holding now. A date you meant to be today is
            // the overwhelmingly common one, and typing it out is the kind of
            // work a form is for: ⏎ ⏎ sets it, and Esc still backs out.
            Some(row)
                if row.control == Control::Date
                    && !row.is_mixed()
                    && row.shown().is_none_or(Value::is_empty) =>
            {
                self.open_editor();
                let now = now_stamp();
                if let Some(ed) = &mut self.editor {
                    ed.set_text(&now);
                }
                self.mode = Mode::Edit;
                self.status = format!("filled with now · {now}");
            }
            Some(_) => {
                self.open_editor();
                self.mode = Mode::Edit;
                self.status.clear();
            }
            None => {}
        }
    }

    /// In Select mode there is no half-typed field to back out of -- Esc in
    /// Edit mode already handled that -- so here Esc and q mean quit. Staged
    /// edits are never discarded silently.
    fn escape(&mut self) {
        // Quitting under a remux would leave its temp file behind; the
        // original is safe either way, but the wait is short and the mess is
        // not. ⌃C still leaves at once.
        if self.writing {
            self.status = "write in progress · wait for it to finish, or ⌃C to leave anyway".into();
            return;
        }
        let n = self.staged_count();
        if n > 0 && !self.confirm_quit {
            self.confirm_quit = true;
            self.status = format!(
                "{} staged edit{} · press again to discard and quit, or w to write",
                n,
                if n == 1 { "" } else { "s" }
            );
        } else {
            self.quit = true;
        }
    }

    /// Step a fixed-set field without opening anything: h/l on an enum cycles
    /// the value, on a rating nudges the stars. Both are one keystroke for the
    /// common case, with the menu still there for picking out of a long list.
    fn nudge(&mut self, delta: isize) {
        let Some(row) = self.rows.get(self.focus) else { return };
        let key = row.key.clone();

        if row.control == Control::Stars {
            let now: u8 = match self.shown_value(row) {
                Some(Value::Text(s)) => s.trim().parse().unwrap_or(0),
                _ => 0,
            };
            let next = (now as isize + delta).clamp(0, 5) as u8;
            self.stage(key, Value::Text(next.to_string()));
            return;
        }

        let mut opts = self.options_for(row);
        if opts.is_empty() {
            return;
        }
        // A set the files disagree about is consolidated before it is
        // stepped: the first h or l puts every file on the answer most of
        // them already hold. Stepping from "no selection" would land on the
        // first option in the list, and the press that ends a disagreement
        // should end it on the likeliest answer, not the alphabetically first.
        if let Agg::Mixed { values } = &row.eff {
            if let Some(code) = majority(values, &opts) {
                let label = opts.iter().find(|o| o.code == code).map_or(code.clone(), |o| o.label.clone());
                let field = row.label.clone();
                let n = values.len();
                self.stage(key, Value::Text(code));
                self.status = format!("{field} · all {n} files on {label} · esc to put them back");
                return;
            }
        }
        let current = match self.shown_value(row) {
            Some(Value::Text(code)) if !code.trim().is_empty() => {
                // Same rule as the editor: a value the set does not know joins
                // it for this field, so stepping off a custom Category can step
                // back onto it.
                match opts.iter().position(|o| o.code == code) {
                    Some(i) => Some(i),
                    None => {
                        opts.push(Opt { code: code.clone(), label: code });
                        Some(opts.len() - 1)
                    }
                }
            }
            _ => None,
        };
        let n = opts.len() as isize;
        // With no value yet, stepping forward lands on the first option and
        // back on the last, rather than jumping to an arbitrary middle.
        let next = match current {
            Some(i) => ((i as isize + delta) % n + n) % n,
            None if delta > 0 => 0,
            None => n - 1,
        } as usize;
        self.stage(key, Value::Text(opts[next].code.clone()));
    }

    /// A rating is the one fixed set small enough to name every member on the
    /// keyboard, so on a Stars row the digits say the value outright: 3 is
    /// three stars from wherever the row stands. h/l still nudge; this is the
    /// same edit without the counting. On any other row the digit is not ours
    /// and falls through to nothing.
    /// A digit in Select mode goes where the focused row can use one: a rating
    /// it names outright, a numeric field it starts typing. On Track that is
    /// the difference between `1` `2` ⏎ meaning twelve and the `1` being
    /// swallowed by a command that does not exist -- the field is digits and
    /// nothing else, so there is no reason to press ⏎ first.
    ///
    /// Seeding replaces rather than appends, the way a digit typed over a
    /// spreadsheet cell does: the keystroke that opened the field is the first
    /// character of a new number, not an edit to the old one. Esc still backs
    /// out to whatever the row showed.
    fn type_digit(&mut self, c: char) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if row.control == Control::Stars {
            if let Some(n) = c.to_digit(6) {
                self.set_stars(n as u8);
            }
            return;
        }
        if !row.def.is_some_and(|d| d.numeric) {
            return;
        }
        self.open_editor();
        if let Some(ed) = &mut self.editor {
            ed.set_text(&c.to_string());
        }
        self.mode = Mode::Edit;
        self.status.clear();
    }

    fn set_stars(&mut self, n: u8) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if row.control != Control::Stars {
            return;
        }
        let key = row.key.clone();
        self.stage(key, Value::Text(n.to_string()));
    }

    /// Stage a value the way an edit would, undo entry and all, on every file
    /// currently in scope.
    fn stage(&mut self, key: String, value: Value) {
        let scope = self.scope();
        self.stage_on(&scope, &key, &value, false);
        self.rebuild_rows();
    }

    /// Put `value` on `targets`, and report how many files took it.
    ///
    /// A file whose own disk value already produces this value through this
    /// control gets no entry: there is nothing to write there. That test is
    /// per file, which is the point -- an edit is not un-made by walking onto
    /// a file that happens to hold the value already.
    ///
    /// With `only_empty`, files that already show something keep it. That is
    /// the backfill: fill the gaps, disturb nothing.
    fn stage_on(&mut self, targets: &[usize], key: &str, value: &Value, only_empty: bool) -> usize {
        let before = self.staged.clone();
        let n = self.place(targets, key, value, only_empty);
        if before != self.staged {
            self.undo.push(before);
            self.redo.clear();
            let sync = self.sync_queue(targets);
            self.note_queue(&sync);
        }
        n
    }

    /// `stage_on` without the undo entry, so a caller placing several values
    /// at once can record them as one step.
    ///
    /// The control is taken from the row where there is one, and from the
    /// schema otherwise -- a fetch may land a value on a field the current
    /// profile hides, and the value still has to be compared through the
    /// right control.
    fn place(&mut self, targets: &[usize], key: &str, value: &Value, only_empty: bool) -> usize {
        let (control, opts) = match self.rows.iter().find(|r| r.key == key) {
            Some(row) => (row.control, self.options_for(row)),
            None => match schema::field_by_id(key) {
                Some(def) => (def.control, Vec::new()),
                None => return 0,
            },
        };
        let mut n = 0;
        for i in targets {
            let Some(file) = self.files.get(*i) else { continue };
            let disk = disk_value(file, key);
            if only_empty {
                let now = overlay(disk.clone(), self.staged.get(i).and_then(|m| m.get(key)));
                if now.is_some_and(|v| !v.is_empty()) {
                    continue;
                }
            }
            // Round-trip the disk value through the same control before
            // comparing: an absent Rating opens as ☆☆☆☆☆, whose value is "0",
            // so comparing against the stored `None` would stage a 0 on every
            // file merely tabbed past.
            let baseline = Editor::new(control, disk.as_ref(), opts.clone()).value();
            let entry = self.staged.entry(*i).or_default();
            if *value == baseline {
                entry.remove(key);
            } else {
                entry.insert(key.to_string(), value.clone());
            }
            n += 1;
        }
        self.staged.retain(|_, edits| !edits.is_empty());
        n
    }

    /// `o` hands the file to the desktop -- `open` on macOS, `xdg-open`
    /// elsewhere. Tagging is a claim about what a file holds, and the one
    /// check the form cannot make is whether the footage is the footage you
    /// think it is.
    ///
    /// Spawned and never waited on: the launcher returns long before the
    /// player does, and this thread owes the event loop a repaint in the
    /// meantime. The aggregate view has no one file to open, so it names the
    /// key that picks one rather than opening an arbitrary member of the
    /// selection.
    fn open_file(&mut self) {
        let idx = match self.view {
            Some(i) => i,
            None if self.files.len() == 1 => 0,
            None => {
                self.status = "no single file in view -- pick one with ] first".into();
                return;
            }
        };
        let path = self.files[idx].path.clone();
        let tool = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let spawned = std::process::Command::new(tool)
            .arg("--")
            .arg(&path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = match spawned {
            Ok(_) => format!("opened {}", file_name(&path)),
            Err(e) => format!("{tool}: {e}"),
        };
    }

    /// Push the focused field out to every open file — over whatever they
    /// hold (`O`, overwrite all), or into only the ones where it is still
    /// empty (`b`, backfill).
    ///
    /// The aggregate view already reaches every file; this is the same reach
    /// from a single-file view, where the value worth spreading is usually the
    /// one just typed onto one file. "Overwrite" rather than "copy" because
    /// that is the half of it worth being warned about: every other file's own
    /// value goes.
    fn copy_out(&mut self, only_empty: bool) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if !row.editable() {
            self.status = format!("{} is read-only", row.label);
            return;
        }
        if self.files.len() < 2 {
            self.status = "only one file open".into();
            return;
        }
        let (key, label) = (row.key.clone(), row.label.clone());
        let Some(value) = row.shown().cloned().filter(|v| !v.is_empty()) else {
            self.status = if row.is_mixed() {
                format!("{label} differs across the selection — pick a file with ] first")
            } else {
                format!("{label} is empty")
            };
            return;
        };
        let all: Vec<usize> = (0..self.files.len()).collect();
        let n = self.stage_on(&all, &key, &value, only_empty);
        self.rebuild_rows();
        let files = format!("{n} file{}", if n == 1 { "" } else { "s" });
        self.status = match (n, only_empty) {
            (0, true) => format!("{label} is already set on every file"),
            (0, false) => format!("nothing to copy into"),
            (_, true) => format!("{label} backfilled into {files}"),
            (_, false) => format!("{label} overwritten on {files}"),
        };
    }

    /// Human label for a stored enum code, so an unfocused Kind row reads
    /// "Movie" rather than the `stik` integer 9 that is actually stored.
    pub fn enum_label(&self, row: &Row, code: &str) -> Option<String> {
        let opts = self.options_for(row);
        if opts.is_empty() {
            return None;
        }
        Some(
            opts.iter()
                .find(|o| o.code == code)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| code.to_string()),
        )
    }

    /// Options for the focused row's enum, if it has one.
    pub fn options_for(&self, row: &Row) -> Vec<Opt> {
        let same = |v: &Vec<String>| {
            v.iter().map(|s| Opt { code: s.clone(), label: s.clone() }).collect::<Vec<_>>()
        };
        match row.key.as_str() {
            "category" => same(&self.enums.category),
            "variant" => same(&self.enums.variant),
            "orientation" => ORIENTATIONS
                .iter()
                .map(|s| Opt { code: (*s).into(), label: (*s).into() })
                .collect(),
            "kind" => KINDS
                .iter()
                .map(|(c, l)| Opt { code: (*c).into(), label: (*l).into() })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Seed a control from the staged edit if there is one, else from disk --
    /// which is what the row's effective aggregate already is.
    fn open_editor(&mut self) {
        let Some(row) = self.rows.get(self.focus) else {
            self.editor = None;
            return;
        };
        let opts = self.options_for(row);
        self.editor = Some(Editor::new(row.control, row.shown(), opts));
    }

    /// Fold the focused control's value into the staging map.
    ///
    /// Compared against what the row was *showing*, not against disk: a
    /// control the user never touched must be a no-op, and a field showing a
    /// staged edit is unchanged when it still reads the same. Comparing
    /// against disk here is what lost edits — walking onto a file that already
    /// held the staged value made the untouched control look like a revert,
    /// and the edit was dropped for every other file with it.
    fn commit_editor(&mut self) {
        let (Some(ed), Some(row)) = (&self.editor, self.rows.get(self.focus)) else { return };
        if !row.editable() {
            return;
        }
        let new = ed.value();
        let shown = Editor::new(row.control, row.shown(), self.options_for(row)).value();
        if new == shown {
            return;
        }
        let key = row.key.clone();
        let typed = match &new {
            Value::Text(s) => s.trim().to_string(),
            _ => String::new(),
        };
        self.stage(key.clone(), new);
        // Place is the row a place is typed into: committing text there is
        // the lookup, and the hit rewrites the row and fills the block. A
        // cleared row is just cleared.
        if key == "location_place" && !typed.is_empty() {
            self.start_lookup(typed);
        }
    }

    pub fn validation(&self) -> Validation {
        self.editor.as_ref().map(|e| e.validate()).unwrap_or(Validation::Ok)
    }

    /// Why the row's staged value cannot be written, if it cannot.
    ///
    /// Asked of a field at rest rather than of one being typed into: a tag set
    /// the write is going to skip has to say so while it is merely sitting
    /// there staged, or the only sign of it is a field that never saves.
    ///
    /// Only staged rows answer. A value already on disk is not something this
    /// run is about to write, and painting it red would be a complaint the
    /// user has no edit to act on.
    pub fn row_error(&self, row: &Row) -> Option<String> {
        if !row.staged {
            return None;
        }
        let values: Vec<&Value> = match &row.eff {
            Agg::Same { value } => vec![value],
            Agg::Mixed { values } => values.iter().flatten().collect(),
            Agg::Absent => vec![],
        };
        values.into_iter().find_map(|v| field_error(&row.key, v))
    }

    /// The value a row should display: the staged edit if any, else what is on
    /// disk. None when the files in scope do not agree.
    pub fn shown_value(&self, row: &Row) -> Option<Value> {
        row.shown().cloned()
    }

    #[cfg(test)]
    /// Put an edit on one file directly, bypassing the control round-trip.
    /// The only way to stage against a file that is not in view, which is what
    /// the tests need and what nothing in the UI does.
    pub fn set_staged(&mut self, file: usize, key: &str, value: Value) {
        self.staged.entry(file).or_default().insert(key.to_string(), value);
        self.rebuild_rows();
    }

    #[cfg(test)]
    /// Put `file` on the write queue with a plan built from its staged edits,
    /// or under the writer, without a writer thread: what the tests need to
    /// see the form answer to a queue, and what nothing in the UI does.
    pub fn enqueue_for_test(&mut self, file: usize, busy: bool) {
        let sound = self.staged.get(&file).cloned().unwrap_or_default();
        let plan = plan::build(&self.files[file], &sound, self.faststart);
        let mut q = lock(&self.queue);
        if busy {
            q.busy = Some(file);
            q.running = true;
        } else {
            let rename = self.rename_after.contains(&file);
            q.waiting.push_back(Job { file, xmp: self.files[file].xmp.clone(), plan, rename });
        }
    }

    #[cfg(test)]
    /// Whether the queued job for `file` will rename it afterwards.
    pub fn queued_rename_for_test(&self, file: usize) -> Option<bool> {
        let q = lock(&self.queue);
        q.waiting.iter().find(|j| j.file == file).map(|j| j.rename)
    }

    #[cfg(test)]
    /// The atoms the queued plan for `file` would write, if it is waiting.
    pub fn queued_atoms_for_test(&self, file: usize) -> Option<Vec<(String, String)>> {
        let q = lock(&self.queue);
        q.waiting.iter().find(|j| j.file == file).map(|j| j.plan.atoms.clone())
    }

    /// Whether a given file carries an edit for a key — the inspector's
    /// question, since it lists the selection file by file.
    pub fn file_is_staged(&self, file: usize, key: &str) -> bool {
        self.staged.get(&file).is_some_and(|m| m.contains_key(key))
    }

    /// After undo or redo the whole map may have moved; every queued file is
    /// brought back in line with it.
    fn sync_all_queued(&mut self) {
        let all: Vec<usize> = (0..self.files.len()).collect();
        self.sync_queue(&all);
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.staged, prev));
            self.sync_all_queued();
            self.rebuild_rows();
            self.status = format!("undo · {} staged", self.staged_count());
        } else {
            self.status = "nothing to undo".into();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.staged, next));
            self.sync_all_queued();
            self.rebuild_rows();
            self.status = format!("redo · {} staged", self.staged_count());
        } else {
            self.status = "nothing to redo".into();
        }
    }

    /// Empty the focused field, staged like any other edit -- so `u` undoes it
    /// and the write plan turns it into a deletion of that key.
    ///
    /// Clearing a field that is already empty stages nothing: `stage` compares
    /// against the disk value through the same control, and an empty value is
    /// what an absent one produces.
    /// Arm the format menu, but only over a field whose value is prose. A case
    /// transform on a rating or an enum code would be a no-op at best, so the
    /// menu refuses to open rather than offering keys that do nothing.
    fn begin_format(&mut self) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if !row.editable() {
            self.status = format!("{} is read-only", row.label);
            return;
        }
        if !is_textual(row.control) {
            self.status = format!("{} takes no formatting", row.label);
            return;
        }
        self.format_pending = true;
        self.status = format!("format: {}", row.label);
    }

    fn apply_case(&mut self, case: Case) {
        let Some(row) = self.rows.get(self.focus) else { return };
        let Some(value) = self.shown_value(row) else {
            self.status = format!("{} is empty", row.label);
            return;
        };
        let recased = match value {
            Value::Text(s) => Value::Text(case.apply(&s)),
            Value::List(l) => Value::List(l.iter().map(|s| case.apply(s)).collect()),
        };
        let (key, label) = (row.key.clone(), row.label.clone());
        self.stage(key, recased);
        self.status = format!("{label} · {}", case.name());
    }

    /// `~` steps the same four cases the `f` menu offers, without the menu.
    ///
    /// Where in the ring it starts is read off the value itself rather than
    /// remembered: whichever case the text is already in, the next press moves
    /// to the one after it, so the key is a toggle you can hold down until the
    /// row looks right instead of a mode you have to track. A case that would
    /// not change the text is skipped -- "Hello" is both capitalized and title
    /// case, and a press that redraws nothing reads as a dead key.
    fn cycle_case(&mut self) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if !row.editable() {
            self.status = format!("{} is read-only", row.label);
            return;
        }
        if !is_textual(row.control) {
            self.status = format!("{} takes no formatting", row.label);
            return;
        }
        let Some(value) = self.shown_value(row) else {
            self.status = format!("{} is empty", row.label);
            return;
        };
        let apply = |case: Case, v: &Value| match v {
            Value::Text(s) => Value::Text(case.apply(s)),
            Value::List(l) => Value::List(l.iter().map(|s| case.apply(s)).collect()),
        };
        let at = CASE_RING.iter().position(|c| apply(*c, &value) == value);
        // Nothing in the ring matches: the text is in some case of its own,
        // and the first press should give an answer rather than guess where in
        // the ring that lands.
        let from = at.map(|i| i + 1).unwrap_or(0);
        let next = (from..from + CASE_RING.len())
            .map(|i| CASE_RING[i % CASE_RING.len()])
            .find(|c| apply(*c, &value) != value);
        let Some(case) = next else {
            self.status = format!("{} reads the same in every case", row.label);
            return;
        };
        let (key, label) = (row.key.clone(), row.label.clone());
        let recased = apply(case, &value);
        self.stage(key, recased);
        self.status = format!("{label} · {}", case.name());
    }

    /// Copy the focused field, staged value and all -- what you see is what
    /// you get, which is the only reading that matches the display.
    fn yank(&mut self) {
        let Some(row) = self.rows.get(self.focus) else { return };
        match self.shown_value(row) {
            Some(v) => {
                self.status = format!("yanked {}", row.label);
                self.clipboard = Some(v);
            }
            None => self.status = format!("{} is empty", row.label),
        }
    }

    /// Paste coerces to the target control rather than refusing across the
    /// text/list split: the same words are meant either way, and the form is
    /// small enough that the two shapes meet constantly.
    fn paste(&mut self) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if !row.editable() {
            self.status = format!("{} is read-only", row.label);
            return;
        }
        let Some(v) = self.clipboard.clone() else {
            self.status = "nothing yanked".into();
            return;
        };
        let listy = matches!(row.control, Control::List | Control::HashTags);
        let value = match (v, listy) {
            (Value::Text(s), true) => Value::List(
                s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            ),
            (Value::List(l), false) => Value::Text(l.join(", ")),
            (v, _) => v,
        };
        let (key, label) = (row.key.clone(), row.label.clone());
        self.stage(key, value);
        self.status = format!("pasted into {label}");
    }

    /// Esc on a fixed set whose choice is staged puts the files back the way
    /// they were -- for a mixed set, back to each file's own answer, which is
    /// the `Original 4  Clip 2` row again. Returns false when there is nothing
    /// to put back, so Esc keeps its ordinary meaning everywhere else.
    ///
    /// Only sets: they are the one control h/l change without opening, so
    /// they are the one place an edit is made without an Esc of its own to
    /// back out of it. Undoable like any other staging change.
    fn revert_focused_set(&mut self) -> bool {
        let Some(row) = self.rows.get(self.focus) else { return false };
        if row.control != Control::Enum || !row.staged {
            return false;
        }
        let (key, label) = (row.key.clone(), row.label.clone());
        let scope = self.scope();
        let before = self.staged.clone();
        for i in &scope {
            if let Some(edits) = self.staged.get_mut(i) {
                edits.remove(&key);
            }
        }
        self.staged.retain(|_, edits| !edits.is_empty());
        if before == self.staged {
            return false;
        }
        self.undo.push(before);
        self.redo.clear();
        let sync = self.sync_queue(&scope);
        self.note_queue(&sync);
        self.rebuild_rows();
        let mixed = self.rows.get(self.focus).is_some_and(|r| r.is_mixed());
        self.status = if mixed {
            format!("{label} put back · each file keeps its own")
        } else {
            format!("{label} put back")
        };
        true
    }

    fn clear_focused(&mut self) {
        let Some(row) = self.rows.get(self.focus) else { return };
        if !row.editable() {
            self.status = format!("{} is read-only", row.label);
            return;
        }
        let empty = match row.control {
            Control::List | Control::HashTags => Value::List(Vec::new()),
            Control::Stars => Value::Text("0".into()),
            _ => Value::Text(String::new()),
        };
        let (key, label) = (row.key.clone(), row.label.clone());
        let was = self.staged.clone();
        self.stage(key, empty);
        self.status = if was == self.staged {
            format!("{label} is already empty")
        } else {
            format!("{label} cleared")
        };
    }

    /// In single-file view the rows describe that file alone, so a value shows
    /// as itself rather than as ‹multiple› -- the aggregate is only meaningful
    /// when more than one file is in scope.
    fn rebuild_rows(&mut self) {
        let scope = self.scope();
        self.rows = build_rows(&self.files, &scope, &self.staged, &self.custom_keys);
        if self.focus >= self.rows.len() {
            self.focus = self.rows.len().saturating_sub(1);
        }
        self.open_editor();
    }

    fn move_focus(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.commit_editor();
        let n = self.rows.len() as isize;
        self.focus = (((self.focus as isize + delta) % n + n) % n) as usize;
        self.open_editor();
    }

    /// Stepping past either end returns to the aggregate view rather than
    /// wrapping, so there is always a way back to "all files" by walking.
    fn cycle_file(&mut self, delta: isize) {
        if self.files.len() < 2 {
            return;
        }
        self.commit_editor();
        let n = self.files.len() as isize;
        self.view = match self.view {
            None if delta > 0 => Some(0),
            None => Some((n - 1) as usize),
            Some(i) => {
                let next = i as isize + delta;
                if next < 0 || next >= n { None } else { Some(next as usize) }
            }
        };
        if let Some(i) = self.view {
            self.request_thumb(i);
        }
        self.rebuild_rows();
        // The view line over the band already says which file this is; the
        // status line saying it too was the same fact twice.
        self.status.clear();
    }
}

/// Visible rows: every primary field, plus footage fields only once they hold
/// something. An absent primary field still gets a row -- seeing that Title is
/// empty is the point of a form.
/// What one file holds for a row key, whether the key is a schema field or an
/// unclaimed atom or XMP tag carried through from disk.
/// Why a staged value cannot be stored under `key`, if it cannot.
///
/// One authority for the question, asked by the form (to paint the field and
/// name the reason) and by the write path (to leave the field out). Only the
/// hashtag grammar has a failure a repair cannot fix; everything else the user
/// can type is storable, which is the point of §5.10's "errors are rare".
pub fn field_error(key: &str, value: &Value) -> Option<String> {
    if field_by_id(key).map(|f| f.control) != Some(Control::HashTags) {
        return None;
    }
    match value {
        Value::List(l) => tag::why_invalid(l),
        Value::Text(s) => tag::why_invalid(&tag::split(s)),
    }
}

pub fn disk_value(t: &FileTags, key: &str) -> Option<Value> {
    match key.split_once(':') {
        Some(("xmp", tag)) => t.xmp.get(tag).cloned(),
        Some((_, k)) => t.atoms.get(k).cloned(),
        None => crate::model::schema::field_by_id(key).and_then(|def| t.lookup(def)),
    }
}

/// A staged edit seen as a value. An empty edit is a *clear*, which reads as
/// absent rather than as an empty string: absent is what the row showed before
/// anyone typed in it, and what the write will leave behind.
fn overlay(disk: Option<Value>, staged: Option<&Value>) -> Option<Value> {
    match staged {
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v.clone()),
        None => disk,
    }
}

/// The label a staged key wears in the confirmation dialog, where there may be
/// no visible row to take it from.
fn key_label(key: &str) -> String {
    if key.contains(':') {
        return custom_label(key);
    }
    crate::model::schema::field_by_id(key)
        .map(|d| d.label.to_string())
        .unwrap_or_else(|| key.to_string())
}

/// Label for an unclaimed key: the namespace prefix is noise once the row is
/// sitting in the Custom group, and `XMP-iptcExt:LocationCreatedGPSLatitude`
/// has to lose something to fit a label column at all.
fn custom_label(key: &str) -> String {
    match key.split_once(':') {
        Some(("xmp", tag)) => tag.rsplit(':').next().unwrap_or(tag).to_string(),
        Some((_, k)) => k.to_string(),
        None => key.to_string(),
    }
}

/// Now, as the Date field wants it: a full local ISO 8601 instant, offset and
/// all. The offset is the point of using local time -- a clip shot at 1am is
/// dated the day you shot it, not the UTC day, which is the mistake a bare
/// `now_utc` makes for half the world for part of every day.
pub fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// The Category the whole selection agrees on, if it agrees on one. A mixed
/// selection has no profile: reshaping the form around one file's answer would
/// hide fields that are set on the others.
/// The one value every file in scope shows for a field -- staged edits laid
/// over disk -- or None when they disagree or none has it. A profile keys off
/// this: reshaping the form around one file's answer would hide fields set on
/// the others.
fn agreed_field(files: &[FileTags], scope: &[usize], staged: &Staged, id: &str) -> Option<String> {
    let def = schema::field_by_id(id)?;
    let mut it = scope
        .iter()
        .map(|i| overlay(files[*i].lookup(def), staged.get(i).and_then(|m| m.get(id))));
    let first = it.next()??;
    it.all(|v| v.as_ref() == Some(&first)).then(|| match first {
        Value::Text(s) => s,
        Value::List(l) => l.join(", "),
    })
}

fn build_rows(
    files: &[FileTags],
    scope: &[usize],
    staged: &Staged,
    custom_keys: &[String],
) -> Vec<Row> {
    // Footage is the first Category to reshape the form rather than merely
    // sitting in it (§3.6): a camera clip has no artist, no channel and no URL
    // to speak of, its people are not actors, and the order it gets filled in
    // is what-then-when-then-who rather than the publishing order the rest of
    // the schema is written in.
    let category = agreed_field(files, scope, staged, "category");
    let footage = category.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(FOOTAGE));
    // Adult is the second: no artist, a publishing order that leads with the
    // channel and the people, and -- for a Clip -- a track number.
    let adult = category.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(ADULT));
    let clip = adult
        && agreed_field(files, scope, staged, "variant")
            .is_some_and(|v| v.eq_ignore_ascii_case(CLIP));

    let row = |key: String, label: String, control, def, disk: Vec<Option<Value>>| {
        let eff = scope
            .iter()
            .zip(disk.iter())
            .map(|(i, d)| overlay(d.clone(), staged.get(i).and_then(|m| m.get(&key))))
            .collect();
        let is_staged =
            scope.iter().any(|i| staged.get(i).is_some_and(|m| m.contains_key(&key)));
        Row { key, label, control, def, eff: Agg::fold(eff), staged: is_staged }
    };

    let mut rows: Vec<Row> = FIELDS
        .iter()
        .filter_map(|def| {
            let disk: Vec<Option<Value>> = scope.iter().map(|i| files[*i].lookup(def)).collect();
            // A footage field appears once it holds something -- or once it has
            // been edited, since hiding the row would hide the edit with it.
            let edited = scope.iter().any(|i| staged.get(i).is_some_and(|m| m.contains_key(def.id)));
            if def.footage_only && !edited && disk.iter().all(Option::is_none) {
                return None;
            }
            // Same escape as above, for the same reason: a hidden row would
            // hide a staged edit that is still going to be written.
            if footage && !edited && FOOTAGE_HIDDEN.contains(&def.id) {
                return None;
            }
            if adult && !edited && ADULT_HIDDEN.contains(&def.id) {
                return None;
            }
            // Offered on an adult file; otherwise only once it holds something.
            if def.adult_only && !adult && !edited && disk.iter().all(Option::is_none) {
                return None;
            }
            // Offered on an adult clip; otherwise only once it holds something.
            if def.clip_only && !clip && !edited && disk.iter().all(Option::is_none) {
                return None;
            }
            let label = match footage.then(|| schema::footage_label(def.id)).flatten() {
                Some(l) => l,
                None => def.label,
            };
            Some(row(def.id.to_string(), label.to_string(), def.control, Some(def), disk))
        })
        .collect();
    // Stable, so the fields the profile does not name keep schema order behind
    // the ones it does.
    if footage {
        rows.sort_by_key(|r| r.def.map_or(usize::MAX, |d| schema::footage_rank(d.id)));
    } else if adult {
        rows.sort_by_key(|r| r.def.map_or(usize::MAX, |d| schema::profile_rank(ADULT_ORDER, d.id)));
    }
    // Keys are already named by origin ("custom:" atom / "xmp:" tag), which the
    // write plan needs in order to put an edit back where it came from.
    rows.extend(custom_keys.iter().map(|k| {
        let disk = scope.iter().map(|i| disk_value(&files[*i], k)).collect();
        row(k.clone(), custom_label(k), Control::Text, None, disk)
    }));
    rows
}

/// One staged edit as the confirmation dialog needs it.
/// What `App::sync_queue` found: how many jobs it rebuilt, and which of the
/// files it was asked about were under the writer and could not be changed.
#[derive(Default)]
struct QueueSync {
    refreshed: usize,
    busy: Vec<usize>,
}

/// Files by where they stand between an edit and the disk (`App::pending`).
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub staged: usize,
    pub queued: usize,
    pub writing: usize,
    /// Flagged to be renamed once their write lands.
    pub rename: usize,
}

/// One field of one file's pending write, as the dialog lists it.
pub struct FileEdit {
    pub label: String,
    /// The edit clears the field rather than setting it.
    pub removed: bool,
    /// Staged, but left out of the write (§5.4).
    pub refused: bool,
}

pub struct StagedEdit {
    pub label: String,
    pub shown: String,
    pub files: usize,
    /// Distinct values on disk this edit is about to replace.
    pub overwrites: usize,
    /// Why the write will leave this field alone, if it will. The dialog is
    /// the last place to see what is about to happen, so an edit that is not
    /// going to happen must not be listed there as though it were.
    pub refused: Option<String>,
}

/// Whether the file now carries what was staged for it. An edit that cleared a
/// field has landed when the field is gone, not when it reads empty-string.
fn landed(t: &FileTags, key: &str, staged: &Value) -> bool {
    match disk_value(t, key) {
        Some(v) => v == *staged || (v.is_empty() && staged.is_empty()),
        None => staged.is_empty(),
    }
}

/// Pick an image backend, querying the terminal only where a reply is plausible.
///
/// `Picker::from_query_stdio()` spawns a thread that blocks reading stdin for a
/// capability response. On a terminal that never answers, the call times out
/// after 2 s -- but that thread stays parked on the read, and then competes with
/// the event loop for keypresses and silently eats them. Driving the app through
/// a plain pty lost roughly half of them that way, which looks like a broken
/// keymap rather than a stuck probe.
///
/// So the query is only issued to terminals that plausibly implement a graphics
/// protocol. Everything else goes straight to halfblocks, which needs no query,
/// spawns no thread, and still draws a picture.
fn make_picker(no_thumbnail: bool) -> ratatui_image::picker::Picker {
    use ratatui_image::picker::Picker;
    if no_thumbnail {
        return Picker::halfblocks();
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let term = env("TERM").to_ascii_lowercase();
    let program = env("TERM_PROGRAM").to_ascii_lowercase();
    let graphical = !env("KITTY_WINDOW_ID").is_empty()
        || !env("WEZTERM_EXECUTABLE").is_empty()
        || term.contains("kitty")
        || term.contains("ghostty")
        || matches!(program.as_str(), "iterm.app" | "wezterm" | "ghostty" | "kitty");
    if graphical {
        Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
    } else {
        Picker::halfblocks()
    }
}

pub fn run(files: Vec<FileTags>, custom: BTreeMap<String, Agg>, no_thumbnail: bool) -> Result<()> {
    let mut terminal = ratatui::init();
    // Ask for the kitty keyboard protocol where the terminal has it: without
    // it ⌘ never reaches a TUI at all, and ⌘S is the save key every other
    // editor on this platform answers to. Terminals without it still get ⌃S.
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
        && crossterm::execute!(
            std::io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            )
        )
        .is_ok();
    let picker = make_picker(no_thumbnail);

    let mut app = App::new(files, custom, !no_thumbnail);
    app.seed_from_filenames();
    app.check_renames((0..app.files.len()).collect());
    let mut proto: Option<ratatui_image::protocol::StatefulProtocol> = None;
    let mut proto_for: Option<usize> = None;

    let res = (|| -> Result<()> {
        loop {
            app.drain();
            // Rebuild the image protocol only when the thumbnail actually
            // changed; doing it per frame would re-encode on every redraw.
            if let (Some(img), Some(idx)) = (&app.thumb_image, app.thumb_for) {
                if proto_for != Some(idx) {
                    proto = Some(picker.new_resize_protocol(img.clone()));
                    proto_for = Some(idx);
                }
            }
            terminal.draw(|f| render::draw(f, &app, proto.as_mut()))?;

            // A running write repaints often enough for the bar to move.
            let tick = if app.writing { 80 } else { 250 };
            if event::poll(Duration::from_millis(tick))? {
                if let Event::Key(key) = event::read()? {
                    app.on_key(key);
                }
            }
            if app.quit {
                return Ok(());
            }
        }
    })();

    if enhanced {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    res
}

use crate::ui::render;

/// Union of every file's values, first-seen order, folded case-insensitively.
///
/// Case folding matters because the same person arrives spelled differently
/// from different sources -- yt-dlp's `%(cast)l`, a hand-typed filename, an XMP
/// list -- and a merge that kept "Alice" and "alice" would make the batch worse
/// rather than better.
pub fn merge_values(per_file: &[Option<Value>]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for v in per_file.iter().flatten() {
        let items = match v {
            Value::List(l) => l.clone(),
            Value::Text(t) => vec![t.clone()],
        };
        for item in items {
            let item = item.trim().to_string();
            if item.is_empty() {
                continue;
            }
            if !merged.iter().any(|m| m.eq_ignore_ascii_case(&item)) {
                merged.push(item);
            }
        }
    }
    merged
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn p(file: usize, total: usize, frac: f64) -> WriteProgress {
        WriteProgress { file, total, label: "", frac }
    }

    #[test]
    fn overall_walks_the_batch_rather_than_resetting_per_file() {
        assert!((p(0, 4, 0.0).overall() - 0.0).abs() < 1e-9);
        assert!((p(0, 4, 1.0).overall() - 0.25).abs() < 1e-9);
        assert!((p(2, 4, 0.5).overall() - 0.625).abs() < 1e-9);
        assert!((p(3, 4, 1.0).overall() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_bad_fraction_cannot_push_the_bar_past_its_ends() {
        assert_eq!(p(0, 1, 9.0).overall(), 1.0);
        assert_eq!(p(0, 1, -1.0).overall(), 0.0);
        assert_eq!(p(0, 0, 0.5).overall(), 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[&str]) -> Option<Value> {
        Some(Value::List(v.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn case_transforms_lower_first_so_a_shouted_value_comes_back_readable() {
        assert_eq!(Case::Capitalize.apply("THE LONG WAY"), "The long way");
        assert_eq!(Case::Title.apply("THE LONG WAY"), "The Long Way");
        assert_eq!(Case::Lower.apply("The Long Way"), "the long way");
        assert_eq!(Case::Upper.apply("The Long Way"), "THE LONG WAY");
    }

    #[test]
    fn the_capital_lands_on_the_letter_not_the_punctuation() {
        assert_eq!(Case::Title.apply("\"foo\" (bar)"), "\"Foo\" (Bar)");
        assert_eq!(Case::Capitalize.apply("  spaced"), "  Spaced");
    }

    /// The point of the minor-word list: a title reads as a title, not as a
    /// row of capitals. First and last word are exempt whatever they are.
    #[test]
    fn title_case_leaves_the_little_words_lowered() {
        assert_eq!(Case::Title.apply("the cat in the hat"), "The Cat in the Hat");
        assert_eq!(Case::Title.apply("what it is for"), "What It Is For");
        assert_eq!(Case::Title.apply("a day at the beach (and a night)"), "A Day at the Beach (and a Night)");
    }

    /// A colon starts a new phrase, and a phrase never opens lowered.
    #[test]
    fn title_case_capitalizes_after_a_colon() {
        assert_eq!(Case::Title.apply("part two: the long way home"), "Part Two: The Long Way Home");
    }

    #[test]
    fn title_case_keeps_the_original_spacing() {
        assert_eq!(Case::Title.apply("a  b\tc"), "A  B\tC");
    }

    #[test]
    fn merge_is_a_union_in_first_seen_order() {
        let got = merge_values(&[l(&["Alice", "Bob"]), l(&["Carol"]), l(&["Dave"])]);
        assert_eq!(got, vec!["Alice", "Bob", "Carol", "Dave"]);
    }

    #[test]
    fn merge_folds_case_and_keeps_the_first_spelling() {
        let got = merge_values(&[l(&["Alice", "Bob"]), l(&["bob", "Carol"])]);
        assert_eq!(got, vec!["Alice", "Bob", "Carol"]);
    }

    #[test]
    fn merge_skips_absent_files_and_blank_entries() {
        let got = merge_values(&[l(&["Alice", "  ", ""]), None, l(&["Bob"])]);
        assert_eq!(got, vec!["Alice", "Bob"]);
    }

    /// An mdta atom holds a list as one comma-joined string on some files and a
    /// real list on others; the merge has to cope with both shapes.
    #[test]
    fn merge_accepts_a_scalar_alongside_lists() {
        let got = merge_values(&[Some(Value::text("Solo")), l(&["Duo"])]);
        assert_eq!(got, vec!["Solo", "Duo"]);
    }

    #[test]
    fn merging_nothing_yields_nothing() {
        assert!(merge_values(&[None, None]).is_empty());
    }

    /// A failed write must not cost the user what they typed: the form is the
    /// only place a staged edit exists, so clearing it turns "try again" into
    /// "type it all again".
    #[test]
    fn a_failed_write_keeps_its_edits() {
        use crate::tags::probe::FileTags;
        let f = FileTags {
            // A path that cannot be probed, so the re-read leaves the fixture
            // as it is and the test stays off the disk.
            path: PathBuf::from("/nonexistent/tagform-test.mov"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(vec![f], BTreeMap::new(), false);
        app.set_staged(0, "title", Value::Text("kept".into()));

        app.finish_write(WriteResults {
            verb: "Wrote",
            ok: vec![],
            failed: vec![(PathBuf::from("/nonexistent/tagform-test.mov"), "boom".into())],
            ..Default::default()
        });

        assert_eq!(app.staged[&0].get("title"), Some(&Value::Text("kept".into())));
        assert!(app.status.contains("1 edit kept"), "{}", app.status);
    }

    /// The other half of the rule: an edit the file now carries is no longer an
    /// edit, or every successful write would leave the form permanently dirty.
    #[test]
    fn an_edit_the_file_now_carries_is_dropped() {
        use crate::tags::probe::FileTags;
        let mut atoms = BTreeMap::new();
        atoms.insert("title".to_string(), Value::Text("landed".into()));
        let f = FileTags {
            path: PathBuf::from("/nonexistent/tagform-test.mov"),
            atoms,
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(vec![f], BTreeMap::new(), false);
        app.set_staged(0, "title", Value::Text("landed".into()));

        app.finish_write(WriteResults {
            verb: "Wrote",
            ok: vec![PathBuf::from("/nonexistent/tagform-test.mov")],
            failed: vec![],
            ..Default::default()
        });

        assert!(app.staged.is_empty(), "{:?}", app.staged);
        assert_eq!(app.status, "wrote 1 of 1");
    }

    /// The reported case, end to end: a tag with a space is repaired on the way
    /// into the staging map, so the field is sound and writes like any other.
    #[test]
    fn tags_typed_with_spaces_are_repaired_into_one_token_each() {
        let mut app = one(&[]);
        let tags = app.rows.iter().position(|r| r.key == "tags").expect("tags row");
        app.jump(tags);
        press(&mut app, KeyCode::Enter);
        for c in "tag, tag two, tag three, another".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);

        assert_eq!(
            shown(&app, "tags"),
            Some(Value::List(vec![
                "another".into(),
                "tag".into(),
                "tag-three".into(),
                "tag-two".into()
            ]))
        );
        let row = app.rows.iter().find(|r| r.key == "tags").unwrap();
        assert_eq!(app.row_error(row), None, "a repaired tag set is writable");
    }

    /// What a repair cannot fix must be visible while the field sits closed --
    /// otherwise the only symptom is a field that quietly never saves.
    #[test]
    fn a_staged_tag_a_repair_cannot_fix_reports_why() {
        let mut app = one(&[]);
        app.set_staged(0, "tags", Value::List(vec!["fine".into(), "a/b".into()]));
        let row = app.rows.iter().find(|r| r.key == "tags").unwrap();
        assert!(app.row_error(row).is_some_and(|e| e.contains("a/b")), "{:?}", app.row_error(row));
    }

    /// A tag set already on disk is not this run's problem: there is no edit to
    /// fix, so there is nothing to complain about.
    #[test]
    fn an_unstaged_value_from_disk_is_not_reported() {
        let app = one(&[("keywords", "a/b")]);
        let row = app.rows.iter().find(|r| r.key == "tags").unwrap();
        assert_eq!(app.row_error(row), None);
    }

    /// The bad field is left out; every other edit still writes, and the status
    /// says which field was dropped and why.
    #[test]
    fn an_unwritable_field_is_dropped_from_the_plan_not_the_whole_write() {
        let mut app = one(&[]);
        app.set_staged(0, "title", Value::text("kept"));
        app.set_staged(0, "tags", Value::List(vec!["a/b".into()]));
        app.prepare_write();

        let plans = app.pending.as_ref().expect("a plan for the sound fields");
        let keys: Vec<&str> = plans[0].atoms.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"title"), "{keys:?}");
        assert!(!keys.contains(&"keywords"), "{keys:?}");
        assert!(app.status.contains("Tags not written"), "{}", app.status);
    }

    /// The dialog is the last look before a write, so it has to agree with
    /// what the write is actually going to do.
    #[test]
    fn the_confirmation_dialog_marks_the_field_it_will_skip() {
        let mut app = one(&[]);
        app.set_staged(0, "title", Value::text("kept"));
        app.set_staged(0, "tags", Value::List(vec!["a/b".into()]));
        let summary = app.staged_summary();
        let by = |l: &str| summary.iter().find(|e| e.label == l).expect(l).refused.clone();
        assert!(by("Tags").is_some_and(|w| w.contains("a/b")));
        assert_eq!(by("Title"), None);
    }

    /// With nothing else staged there is no plan at all -- and the reason has
    /// to survive, or the write reads as "nothing to write" when there was.
    #[test]
    fn a_write_of_only_an_unwritable_field_says_why_it_did_nothing() {
        let mut app = one(&[]);
        app.set_staged(0, "tags", Value::List(vec!["a/b".into()]));
        app.prepare_write();
        assert!(app.pending.is_none());
        assert!(app.status.contains("Tags not written"), "{}", app.status);
        assert!(app.status.contains("a/b"), "{}", app.status);
    }

    /// One unprobeable file with whatever atoms the test wants, so nothing
    /// here touches a disk.
    fn one(atoms: &[(&str, &str)]) -> App {
        use crate::tags::probe::FileTags;
        let f = FileTags {
            path: PathBuf::from("/nonexistent/tagform-test.mov"),
            atoms: atoms.iter().map(|(k, v)| (k.to_string(), Value::text(*v))).collect(),
            xmp: BTreeMap::new(),
        };
        App::new(vec![f], BTreeMap::new(), false)
    }

    /// On open, a name with structure fills the fields the file leaves
    /// empty -- staged, so it is green until written, and one `u` takes it
    /// all back. A field the file already holds keeps its value, and a bare
    /// stem is not read as a title.
    #[test]
    fn a_structured_filename_seeds_the_empty_fields_on_open() {
        use crate::tags::probe::FileTags;
        let mk = |name: &str, atoms: &[(&str, &str)]| FileTags {
            path: PathBuf::from(format!("/nonexistent/{name}")),
            atoms: atoms.iter().map(|(k, v)| (k.to_string(), Value::text(*v))).collect(),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(
            vec![
                mk("Ann (Studio) - A Title #pov ★★★☆☆.mp4", &[("title", "Kept")]),
                mk("IMG_0412.mov", &[]),
            ],
            BTreeMap::new(),
            false,
        );
        app.seed_from_filenames();
        let seeded = app.staged.get(&0).expect("the structured name seeds");
        assert_eq!(seeded.get("actors"), Some(&Value::List(vec!["Ann".into()])));
        assert_eq!(seeded.get("channel"), Some(&Value::text("Studio")));
        assert_eq!(seeded.get("tags"), Some(&Value::List(vec!["pov".into()])));
        assert_eq!(seeded.get("rating"), Some(&Value::text("3")));
        assert!(!seeded.contains_key("title"), "the file's own title stands");
        assert!(!app.staged.contains_key(&1), "a bare stem is not a title");
        assert!(app.status.contains("from the filenames"), "{}", app.status);
        app.undo();
        assert!(app.staged.is_empty(), "one step back");
    }

    /// A name that tells the form nothing it lacks says nothing on open.
    #[test]
    fn a_filename_with_nothing_new_seeds_nothing_and_says_nothing() {
        let mut app = one(&[]);
        app.seed_from_filenames();
        assert!(app.staged.is_empty());
        assert!(app.status.is_empty(), "{}", app.status);
    }

    /// A file's alerts: a rename that would land on another file, and a
    /// staged value the write will leave out.
    #[test]
    fn a_files_alerts_name_a_taken_rename_and_a_refused_value() {
        let mut app = one(&[]);
        assert!(app.file_alerts(0).is_empty());
        app.conflicts.insert(0, PathBuf::from("/nonexistent/other.mov"));
        app.set_staged(0, "tags", Value::List(vec!["pov".into(), ".bad".into()]));
        let alerts = app.file_alerts(0);
        assert_eq!(alerts.len(), 2, "{alerts:?}");
        assert!(alerts[0].contains("already named other.mov"), "{alerts:?}");
        assert!(alerts[1].contains(".bad"), "{alerts:?}");
    }

    /// A conflict answer is for the path that was asked about. One that
    /// arrives after the file moved is about a name it no longer has.
    #[test]
    fn a_conflict_answer_for_a_name_the_file_has_left_is_dropped() {
        let mut app = one(&[]);
        let here = app.files[0].path.clone();
        let taken = PathBuf::from("/nonexistent/taken.mov");
        app.tx.send(Msg::Conflict(0, PathBuf::from("/nonexistent/old.mov"), Some(taken.clone()))).unwrap();
        app.drain();
        assert!(app.conflicts.is_empty(), "stale");
        app.tx.send(Msg::Conflict(0, here.clone(), Some(taken.clone()))).unwrap();
        app.drain();
        assert_eq!(app.conflicts.get(&0), Some(&taken));
        app.tx.send(Msg::Conflict(0, here, None)).unwrap();
        app.drain();
        assert!(app.conflicts.is_empty(), "cleared once the name is free");
    }

    /// Which file is in view is the view line's to say; the status line
    /// does not repeat it.
    #[test]
    fn stepping_between_files_leaves_the_status_line_alone() {
        use crate::tags::probe::FileTags;
        let mk = |i: usize| FileTags {
            path: PathBuf::from(format!("/nonexistent/clip-{i}.mov")),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(vec![mk(0), mk(1)], BTreeMap::new(), false);
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.view, Some(0));
        assert!(app.status.is_empty(), "{}", app.status);
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.view, None);
        assert!(app.status.is_empty(), "{}", app.status);
    }

    fn keys(app: &App) -> Vec<&str> {
        app.rows.iter().map(|r| r.key.as_str()).collect()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn shown(app: &App, id: &str) -> Option<Value> {
        app.rows.iter().find(|r| r.key == id).and_then(|r| r.shown().cloned())
    }

    /// A fetch takes the page's word for the fields it answered and leaves
    /// every other field alone -- and the whole of it is one undo step.
    #[test]
    fn a_fetch_fills_what_the_page_answered_and_keeps_the_rest() {
        let mut app = one(&[("title", "Old Title"), ("channel", "Old Channel")]);
        app.finish_fetch(vec![(
            0,
            Ok(vec![("title", Value::text("New Title")), ("tags", Value::List(vec!["a".into()]))]),
        )]);
        assert_eq!(shown(&app, "title"), Some(Value::text("New Title")));
        assert_eq!(shown(&app, "channel"), Some(Value::text("Old Channel")));
        assert_eq!(shown(&app, "tags"), Some(Value::List(vec!["a".into()])));
        assert_eq!(app.status, "fetched 2 fields");
        press(&mut app, KeyCode::Char('u'));
        assert!(app.staged.is_empty(), "{:?}", app.staged);
        assert_eq!(shown(&app, "title"), Some(Value::text("Old Title")));
        assert_eq!(shown(&app, "tags"), None);
    }

    /// A page that failed reports why, and stages nothing.
    #[test]
    fn a_failed_fetch_stages_nothing() {
        let mut app = one(&[("title", "T")]);
        app.finish_fetch(vec![(0, Err("Video unavailable".into()))]);
        assert!(app.staged.is_empty());
        assert_eq!(app.status, "Video unavailable");
    }

    /// Five stars is five keys away by nudging and one key away by naming.
    /// Outside a rating and a numeric field the digit stages nothing rather
    /// than typing into a field nobody opened.
    #[test]
    fn a_digit_sets_the_rating_and_leaves_other_rows_alone() {
        let mut app = one(&[("rating", "2")]);
        let rating = app.rows.iter().position(|r| r.key == "rating").unwrap();
        app.jump(rating);
        press(&mut app, KeyCode::Char('5'));
        assert_eq!(shown(&app, "rating"), Some(Value::Text("5".into())));
        press(&mut app, KeyCode::Char('0'));
        assert_eq!(shown(&app, "rating"), Some(Value::Text("0".into())));
        // Back to what disk holds: nothing left staged to write.
        press(&mut app, KeyCode::Char('2'));
        assert!(app.staged.is_empty());

        let title = app.rows.iter().position(|r| r.key == "title").unwrap();
        app.jump(title);
        press(&mut app, KeyCode::Char('3'));
        assert!(app.staged.is_empty());
        assert_eq!(app.mode, Mode::Select);
    }

    /// Track is digits and nothing else, so a digit on the row is the value:
    /// it opens the field already holding that number, further digits type on
    /// as normal, and ⏎ commits. The keystroke replaces what was there -- `1`
    /// on a track reading 7 starts a new number, it does not make 71.
    #[test]
    fn a_digit_on_track_starts_typing_the_number() {
        let mut app = one(&[("track", "7")]);
        let track = app.rows.iter().position(|r| r.key == "track").expect("track row");
        app.jump(track);

        press(&mut app, KeyCode::Char('1'));
        assert_eq!(app.mode, Mode::Edit);
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Select);
        assert_eq!(shown(&app, "track"), Some(Value::Text("12".into())));

        // One digit is one key and ⏎, with no trace of the old value.
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, "track"), Some(Value::Text("3".into())));

        // Esc backs out to what the row showed, staged value and all.
        press(&mut app, KeyCode::Char('9'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(shown(&app, "track"), Some(Value::Text("3".into())));
    }

    /// `i u` reads the URL field wherever the cursor is, and on an empty URL
    /// it has nothing to ask -- so it does not start yt-dlp.
    #[test]
    fn fetch_works_from_any_field_and_needs_only_a_url() {
        let mut app = one(&[("title", "T")]);
        app.jump(0);
        press(&mut app, KeyCode::Char('i'));
        assert!(app.import_menu);
        press(&mut app, KeyCode::Char('u'));
        assert!(!app.import_menu);
        assert!(!app.fetching);
        assert_eq!(app.status, "no URL to fetch from");
    }

    /// ⌃N / ⌃P walk the selection the way `]` / `[` do. The bracket keys stay:
    /// this is the pair the hand already knows from every other list on the
    /// machine, not a replacement.
    #[test]
    fn ctrl_n_and_ctrl_p_walk_the_selection() {
        let mut app = pair();
        assert_eq!(app.view, None);
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.view, Some(0));
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.view, Some(1));
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.view, Some(0));
    }

    /// ⌘Z is `u` under the name the rest of the machine uses, and ⌘⇧Z is ⌃R.
    #[test]
    fn cmd_z_undoes_and_cmd_shift_z_redoes() {
        let mut app = one(&[("title", "before")]);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("after"));
        assert_eq!(shown(&app, "title"), Some(Value::text("after")));
        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::SUPER));
        assert_eq!(shown(&app, "title"), Some(Value::text("before")));
        app.on_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SUPER));
        assert_eq!(shown(&app, "title"), Some(Value::text("after")));
    }

    /// A rating is a fixed set, so it opens no more than one does: h/l nudge
    /// it, 0-5 name it, and j/k stay the keys that leave the row.
    #[test]
    fn enter_never_opens_a_rating() {
        let mut app = one(&[("rating", "2")]);
        focus_on(&mut app, "rating");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Select);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(shown(&app, "rating"), Some(Value::text("3")));
        press(&mut app, KeyCode::Char('h'));
        assert!(app.staged.is_empty(), "back to what disk holds");
        press(&mut app, KeyCode::Char('5'));
        assert_eq!(shown(&app, "rating"), Some(Value::text("5")));
        // j leaves the row rather than typing a letter into it.
        let was = app.focus;
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.focus, was + 1);
    }

    /// `~` walks the four cases without the menu, reading where it is in the
    /// ring off the text itself -- and never lands on a press that redraws
    /// nothing, which is what a case the value is already in would be.
    #[test]
    fn tilde_steps_the_case_ring_and_skips_the_no_ops() {
        let mut app = one(&[("title", "the SHAPE of water")]);
        focus_on(&mut app, "title");
        let mut seen = Vec::new();
        for _ in 0..5 {
            press(&mut app, KeyCode::Char('~'));
            seen.push(match shown(&app, "title") {
                Some(Value::Text(s)) => s,
                other => panic!("{other:?}"),
            });
        }
        assert_eq!(
            seen,
            vec![
                "The shape of water",
                "The Shape of Water",
                "the shape of water",
                "THE SHAPE OF WATER",
                "The shape of water",
            ],
            "the ring must cycle, not stall"
        );
    }

    /// A rating has no case to step, and saying so beats staging nonsense.
    #[test]
    fn tilde_refuses_a_field_with_no_case() {
        let mut app = one(&[("rating", "3")]);
        focus_on(&mut app, "rating");
        press(&mut app, KeyCode::Char('~'));
        assert!(app.staged.is_empty());
        assert!(app.status.contains("takes no formatting"), "{}", app.status);
    }

    /// The import menu owns every key while it is up: a key that means
    /// something elsewhere must not reach the form through it, and one that
    /// means nothing here must not throw the menu away either.
    #[test]
    fn the_import_menu_is_modal() {
        let mut app = one(&[("title", "T")]);
        press(&mut app, KeyCode::Char('i'));
        assert!(app.import_menu);
        press(&mut app, KeyCode::Char('w'));
        assert!(app.import_menu, "an unrecognised key must not close the menu");
        assert!(app.pending.is_none(), "w inside the menu must not open a write plan");
        press(&mut app, KeyCode::Esc);
        assert!(!app.import_menu);
        assert_eq!(app.status, "import cancelled");
        press(&mut app, KeyCode::Char('I'));
        assert!(app.inspector);
    }

    /// j/k choose and ⏎ runs, so the source can be picked while reading its
    /// preview rather than by knowing its letter.
    #[test]
    fn the_import_menu_is_a_selector() {
        use crate::tags::probe::FileTags;
        let f = FileTags {
            path: PathBuf::from("/x/Ann Lee (Studio X) - A Title.mp4"),
            atoms: [("webpage_url".to_string(), Value::text("https://example.com/v"))]
                .into_iter()
                .collect(),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(vec![f], BTreeMap::new(), false);
        press(&mut app, KeyCode::Char('i'));
        // A file with a URL opens on the URL, and j walks to the filename.
        assert_eq!(app.import_pick, ImportSource::Url);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.import_pick, ImportSource::Filename);
        // k comes back, and wraps the way the form's k does -- onto the
        // place lookup, the third source; two j's return to the filename.
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.import_pick, ImportSource::Url);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.import_pick, ImportSource::Location);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.import_pick, ImportSource::Filename);
        // ⏎ runs the source under the cursor -- here the filename, which is
        // synchronous, so the staging is on the far side of the keystroke.
        press(&mut app, KeyCode::Enter);
        assert!(!app.import_menu);
        assert_eq!(shown(&app, "channel"), Some(Value::text("Studio X")));
    }

    /// With no URL to fetch from, ⏎ on a freshly opened menu must not land on
    /// the one source that can only refuse.
    #[test]
    fn the_import_menu_opens_on_a_source_with_something_to_offer() {
        let mut app = one(&[("title", "T")]);
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.import_pick, ImportSource::Filename);
    }

    fn hit(name: &str, city: &str) -> Hit {
        Hit {
            name: name.into(),
            city: city.into(),
            state: "Metro Manila".into(),
            country: "Philippines".into(),
            lat: 14.5641,
            lon: 121.03,
        }
    }

    /// `i l` opens the prompt over the location block as it stands, so a
    /// half-filled block is one ⏎ from being looked up.
    #[test]
    fn the_lookup_prompt_opens_on_what_the_block_already_says() {
        let mut app = one(&[("title", "T")]);
        app.stage("location".into(), Value::text("Makati"));
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('l'));
        assert!(!app.import_menu);
        match &app.locate {
            Some(Locate::Ask(ed)) => assert_eq!(ed.value(), Value::text("Makati")),
            _ => panic!("the prompt should be open"),
        }
        press(&mut app, KeyCode::Esc);
        assert!(app.locate.is_none());
        assert_eq!(app.status, "lookup cancelled");
    }

    /// Place is always in the form, and ⏎ on it is the lookup prompt -- the
    /// same one `i l` opens -- over the block as it stands. ⏎ again runs the
    /// helper, and the hit fills the row and the block beneath it.
    #[test]
    fn enter_on_place_opens_the_lookup() {
        let mut app = one(&[("title", "T")]);
        let at = app.rows.iter().position(|r| r.key == "location_place").expect("Place is always shown");
        assert_eq!(shown(&app, "location"), None, "the rest of the block waits for a hit");
        app.jump(at);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.locate, Some(Locate::Ask(_))), "⏎ on Place is the prompt, not an inline edit");
        assert_eq!(app.mode, Mode::Select);
        for c in "Coro Hotel Makati".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.locate, Some(Locate::Looking)), "{}", app.status);
        assert_eq!(app.status, "looking up Coro Hotel Makati");
        app.finish_locate(Ok(vec![hit("Coro Hotel", "Makati")]), true);
        assert_eq!(shown(&app, "location_place"), Some(Value::text("Coro Hotel")));
        assert_eq!(shown(&app, "location"), Some(Value::text("Makati")));
        assert_eq!(shown(&app, "coordinates"), Some(Value::text("+14.5641+121.0300/")));
        // Clearing the row is not a lookup.
        app.stage("location_place".into(), Value::text(""));
        assert!(app.locate.is_none());
    }

    /// An empty prompt on a file with no coordinates has nothing to ask, so
    /// it stays open and says so rather than starting the helper.
    #[test]
    fn an_empty_lookup_with_nothing_to_go_on_refuses() {
        let mut app = one(&[("title", "T")]);
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.locate, Some(Locate::Ask(_))));
        assert!(app.status_error);
    }

    /// One hit is staged straight onto the whole block, as one undo step;
    /// a typed lookup keeps the venue name, and the row it fills appears.
    #[test]
    fn a_single_hit_stages_the_block_and_is_one_undo() {
        let mut app = one(&[("title", "T")]);
        app.locate = Some(Locate::Looking);
        app.finish_locate(Ok(vec![hit("Coro Hotel", "Makati")]), true);
        assert!(app.locate.is_none());
        assert_eq!(shown(&app, "location_place"), Some(Value::text("Coro Hotel")));
        assert_eq!(shown(&app, "location"), Some(Value::text("Makati")));
        assert_eq!(shown(&app, "location_country"), Some(Value::text("Philippines")));
        assert_eq!(shown(&app, "coordinates"), Some(Value::text("+14.5641+121.0300/")));
        assert!(app.status.starts_with("Coro Hotel, Makati"), "{}", app.status);
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(shown(&app, "location"), None);
    }

    /// ⏎ on a Coordinates row that holds something runs the reverse lookup
    /// at once; a reverse hit names the city but not the nearest venue.
    #[test]
    fn a_reverse_hit_leaves_the_venue_alone() {
        let mut app = one(&[("com.apple.quicktime.location.iso6709", "+14.5641+121.0300/")]);
        assert_eq!(app.import_preview().coords, Some((14.5641, 121.03)));
        let at = app.rows.iter().position(|r| r.key == "coordinates").unwrap();
        app.jump(at);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.locate, Some(Locate::Looking)), "{}", app.status);
        assert_eq!(app.mode, Mode::Select);
        app.finish_locate(Ok(vec![hit("Some Shop", "Makati")]), false);
        assert_eq!(shown(&app, "location_place"), None);
        assert_eq!(shown(&app, "location"), Some(Value::text("Makati")));
    }

    /// Several hits are chosen from: j/k walk them, ⏎ takes the one under
    /// the cursor, and nothing is staged until then.
    #[test]
    fn several_hits_are_picked_from() {
        let mut app = one(&[("title", "T")]);
        app.locate = Some(Locate::Looking);
        app.finish_locate(Ok(vec![hit("Coro Hotel", "Makati"), hit("Coro Cafe", "Pasay")]), true);
        assert!(matches!(app.locate, Some(Locate::Pick { at: 0, .. })));
        assert_eq!(shown(&app, "location"), None);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(shown(&app, "location_place"), Some(Value::text("Coro Cafe")));
        assert_eq!(shown(&app, "location"), Some(Value::text("Pasay")));
    }

    /// Esc during the wait drops the answer when it comes, and a refusal is
    /// an error in the status line, not a silent nothing.
    #[test]
    fn a_cancelled_lookup_drops_its_answer() {
        let mut app = one(&[("title", "T")]);
        app.locate = Some(Locate::Looking);
        press(&mut app, KeyCode::Esc);
        app.finish_locate(Ok(vec![hit("Coro Hotel", "Makati")]), true);
        assert_eq!(shown(&app, "location"), None);
        app.locate = Some(Locate::Looking);
        app.finish_locate(Err("no hits".into()), true);
        assert!(app.locate.is_none());
        assert!(app.status_error);
        assert_eq!(app.status, "no hits");
    }

    /// A source that could not answer says so in the error colour: the words
    /// alone were being read as one more grey note.
    #[test]
    fn a_refused_import_is_marked_an_error() {
        let mut app = one(&[("title", "T")]);
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.status, "no URL to fetch from");
        assert!(app.status_error);
        // The next key is the acknowledgement, and the colour goes with it.
        press(&mut app, KeyCode::Char('j'));
        assert!(!app.status_error);

        app.finish_fetch(vec![(0, Err("Video unavailable".into()))]);
        assert!(app.status_error);
        assert_eq!(app.status, "Video unavailable");
    }

    /// `i f` fills the empty fields from the name and leaves a field that
    /// already holds something alone -- and the whole of it is one undo step.
    #[test]
    fn a_filename_import_fills_only_the_empty_fields() {
        use crate::tags::probe::FileTags;
        let f = FileTags {
            path: PathBuf::from("/x/Ann Lee, Bo Cruz (Studio X) - A Title #pov #hd ★★★★☆.mp4"),
            atoms: [("title".to_string(), Value::text("Kept Title"))].into_iter().collect(),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(vec![f], BTreeMap::new(), false);
        let p = app.import_preview();
        assert_eq!(p.keeps, ["Title"]);
        assert_eq!(p.fills.len(), 4, "{:?}", p.fills);
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(shown(&app, "title"), Some(Value::text("Kept Title")));
        assert_eq!(shown(&app, "actors"), Some(Value::List(vec!["Ann Lee".into(), "Bo Cruz".into()])));
        assert_eq!(shown(&app, "channel"), Some(Value::text("Studio X")));
        assert_eq!(shown(&app, "rating"), Some(Value::text("4")));
        assert_eq!(shown(&app, "tags"), Some(Value::List(vec!["pov".into(), "hd".into()])));
        assert_eq!(app.status, "imported 4 fields");
        press(&mut app, KeyCode::Char('u'));
        assert!(app.staged.is_empty(), "{:?}", app.staged);
        // A second import has nothing left to fill, and says so.
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('f'));
        press(&mut app, KeyCode::Char('u'));
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('f'));
        assert!(app.status.starts_with("imported"), "{}", app.status);
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.status, "nothing new: every field the name carries is already set");
    }

    /// A name with nothing to read says so and stages nothing.
    #[test]
    fn a_bare_filename_imports_nothing() {
        use crate::tags::probe::FileTags;
        let f = FileTags {
            path: PathBuf::from("/x/[1080p 30fps].mp4"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(vec![f], BTreeMap::new(), false);
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Char('f'));
        assert!(app.staged.is_empty());
        assert!(app.status.starts_with("nothing recognised in"), "{}", app.status);
    }

    /// Footage is a different form, not the same form with a label on it: the
    /// fields a camera file has no use for go, and the ones it lives by come
    /// first.
    #[test]
    fn the_footage_category_reshapes_the_form() {
        let app = one(&[("category", "Footage")]);
        let k = keys(&app);
        for hidden in FOOTAGE_HIDDEN {
            assert!(!k.contains(hidden), "{hidden} should be hidden: {k:?}");
        }
        let head: Vec<&str> = k.iter().take(9).copied().collect();
        assert_eq!(
            head,
            ["category", "variant", "date", "actors", "rating", "tags", "location_place", "title", "description"]
        );
        // The fields the profile does not name keep their schema order behind
        // the ones it does.
        assert_eq!(k[9..], ["genre", "kind", "origin"]);
        assert_eq!(row(&app, "actors").label, "People");
    }

    /// And any other Category leaves it alone -- including none at all, which
    /// is what most files carry.
    #[test]
    fn every_other_category_keeps_the_ordinary_form() {
        for app in [one(&[]), one(&[("category", "Music Video")])] {
            let k = keys(&app);
            assert_eq!(k[..3], ["category", "variant", "title"], "{k:?}");
            for hidden in FOOTAGE_HIDDEN {
                assert!(k.contains(hidden), "{hidden} should be shown: {k:?}");
            }
            assert_eq!(row(&app, "actors").label, "Actors");
        }
    }

    /// Adult is the second profile: no Artist row, an Orientation set under
    /// the other two, the publishing order, and no Track until the file is a
    /// Clip.
    #[test]
    fn the_adult_category_reshapes_the_form() {
        let app = one(&[("category", "Adult")]);
        let k = keys(&app);
        assert!(!k.contains(&"artist"), "{k:?}");
        assert!(!k.contains(&"track"), "not a clip: {k:?}");
        assert_eq!(
            k[..14],
            [
                "category", "variant", "orientation", "title", "channel", "actors", "rating",
                "url", "tags", "date", "description", "genre", "synopsis", "origin"
            ]
        );
        assert_eq!(k[14..], ["kind"]);
        let opts: Vec<String> =
            app.options_for(row(&app, "orientation")).into_iter().map(|o| o.code).collect();
        assert_eq!(opts, ["Straight", "Gay", "Sapphic", "Trans"]);

        let app = one(&[("category", "Adult"), ("variant", "Clip")]);
        let k = keys(&app);
        assert_eq!(k[..5], ["category", "variant", "orientation", "title", "track"], "{k:?}");
        assert_eq!(row(&app, "track").label, "Track");
    }

    /// Orientation belongs to the adult profile alone: absent elsewhere until
    /// a file actually carries one, and then kept -- hiding it would hide a
    /// key the write carries (invariant 4).
    #[test]
    fn orientation_is_offered_only_to_adult_files_unless_present() {
        for app in [one(&[]), one(&[("category", "Music Video")])] {
            assert!(!keys(&app).contains(&"orientation"), "{:?}", keys(&app));
        }
        let app = one(&[("category", "Footage"), ("orientation", "Gay")]);
        assert!(keys(&app).contains(&"orientation"), "{:?}", keys(&app));
    }

    /// Choosing Clip in the form, not just on disk, brings Track in -- and
    /// a track number already on a file shows regardless of the profile.
    #[test]
    fn track_follows_the_staged_variant_and_its_own_value() {
        let mut app = one(&[("category", "Adult")]);
        assert!(!keys(&app).contains(&"track"));
        app.set_staged(0, "variant", Value::text("Clip"));
        assert!(keys(&app).contains(&"track"), "{:?}", keys(&app));

        let app = one(&[("category", "Music Video"), ("track", "3")]);
        assert!(keys(&app).contains(&"track"), "{:?}", keys(&app));
        assert!(!keys(&one(&[("category", "Music Video")])).contains(&"track"));
    }

    /// ⌃S / ⌘S is save from either mode: it commits the open field first,
    /// so the plan holds what was on screen, and leaves Edit mode behind.
    #[test]
    fn save_hotkey_commits_the_open_field_and_prepares_the_write() {
        let mut app = one(&[]);
        app.focus = app.rows.iter().position(|r| r.key == "title").unwrap();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Edit);
        for c in "hi".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.mode, Mode::Select);
        assert_eq!(app.staged_count(), 1);
        assert!(app.pending.is_some(), "{}", app.status);

        let mut app = one(&[]);
        app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::SUPER));
        assert!(app.pending.is_none());
        assert_eq!(app.status, "nothing to write");
    }

    /// Hiding a row must never hide a pending write. Same escape the
    /// footage-only fields have, for the same reason.
    #[test]
    fn a_staged_edit_keeps_its_row_even_under_the_footage_profile() {
        let mut app = one(&[("category", "Footage")]);
        assert!(!keys(&app).contains(&"url"));
        app.set_staged(0, "url", Value::text("https://example.com/a"));
        assert!(keys(&app).contains(&"url"), "an invisible edit would still be written");
    }

    /// A selection that disagrees about Category has no profile: reshaping the
    /// form around one file's answer would hide fields set on the others.
    #[test]
    fn a_mixed_category_gets_no_profile() {
        use crate::tags::probe::FileTags;
        let mk = |cat: &str| FileTags {
            path: PathBuf::from(format!("/nonexistent/{cat}.mov")),
            atoms: BTreeMap::from([("category".to_string(), Value::text(cat))]),
            xmp: BTreeMap::new(),
        };
        let app = App::new(vec![mk("Footage"), mk("Meme")], BTreeMap::new(), false);
        assert!(keys(&app).contains(&"url"));
    }

    /// A set has no open state: ⏎ on one says which keys work instead of
    /// putting the form into a mode whose only key is h/l.
    #[test]
    fn enter_never_opens_a_fixed_set() {
        let mut app = one(&[]);
        for key in ["category", "variant", "kind"] {
            focus_on(&mut app, key);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.mode, Mode::Select, "{key} opened a mode");
        }
        // And h/l still step it in place, from the mode it never left.
        focus_on(&mut app, "category");
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(row(&app, "category").shown(), Some(&Value::text(&app.enums.category[0])));
    }

    /// ⏎ on an empty Date fills in now rather than an empty line to type into.
    /// The value is staged only once it is accepted, so the field is still an
    /// edit you can back out of.
    #[test]
    fn enter_on_an_empty_date_fills_in_now() {
        let mut app = one(&[]);
        focus_on(&mut app, "date");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Edit);
        let now = now_stamp();
        let shown = app.editor.as_ref().unwrap().display().0;
        assert_eq!(shown[..10], now[..10], "today's date, in full: {shown}");
        assert_eq!(app.validation(), Validation::Ok, "{shown} must not paint as a warning");
        assert!(app.staged.is_empty(), "nothing is staged until it is accepted");

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.staged[&0].get("date"), Some(&Value::text(&shown)));
    }

    /// A Date that already holds something opens holding that, not now --
    /// overwriting an authored capture time with the moment you pressed ⏎ is
    /// the one thing this must never do.
    #[test]
    fn enter_on_a_filled_date_leaves_it_alone() {
        let mut app = one(&[("date", "1999-12-31")]);
        focus_on(&mut app, "date");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.as_ref().unwrap().display().0, "1999-12-31");
        press(&mut app, KeyCode::Enter);
        assert!(app.staged.is_empty(), "{:?}", app.staged);
    }

    /// Two files whose Title differs -- the shape every multi-file bug shows
    /// up in.
    fn pair() -> App {
        use crate::tags::probe::FileTags;
        let mk = |name: &str, title: Option<&str>| FileTags {
            // Paths that cannot be probed, so nothing here touches a disk.
            path: PathBuf::from(format!("/nonexistent/{name}.mov")),
            atoms: title
                .map(|t| BTreeMap::from([("title".to_string(), Value::text(t))]))
                .unwrap_or_default(),
            xmp: BTreeMap::new(),
        };
        App::new(vec![mk("a", Some("A")), mk("b", Some("B"))], BTreeMap::new(), false)
    }

    fn focus_on(app: &mut App, key: &str) {
        app.focus = app.rows.iter().position(|r| r.key == key).expect(key);
        app.open_editor();
    }

    fn row<'a>(app: &'a App, key: &str) -> &'a Row {
        app.rows.iter().find(|r| r.key == key).expect(key)
    }

    /// The reported bug: walking to another file and back reset a field to
    /// what it held on read. It happened whenever the file walked onto already
    /// agreed with the edit -- which is the normal case when giving a batch
    /// the same value one file at a time.
    #[test]
    fn an_edit_survives_a_file_that_already_agrees_with_it() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("B"));

        app.cycle_file(1); // onto the file that already reads "B"
        app.move_focus(1);
        app.move_focus(-1);
        app.cycle_file(-1);

        assert_eq!(app.staged[&0].get("title"), Some(&Value::text("B")));
        assert_eq!(row(&app, "title").shown(), Some(&Value::text("B")));
    }

    /// And it belongs to the file it was made on: the next file shows its own
    /// value, not the edit trailing behind the cursor.
    #[test]
    fn an_edit_does_not_follow_the_cursor_onto_the_next_file() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("edited"));

        app.cycle_file(1);
        assert_eq!(row(&app, "title").shown(), Some(&Value::text("B")));
        assert!(!row(&app, "title").staged);

        app.cycle_file(-1);
        assert_eq!(row(&app, "title").shown(), Some(&Value::text("edited")));
        assert!(row(&app, "title").staged);
    }

    /// ⏎ on a field of a file still waiting in the queue is the save: the
    /// file's job is rebuilt around the new value, so it is written once, with
    /// everything, and the user is told so.
    #[test]
    fn an_edit_to_a_queued_file_is_folded_into_its_write() {
        let mut app = pair();
        app.cycle_file(1); // a.mov
        app.set_staged(0, "channel", Value::text("first"));
        app.enqueue_for_test(0, false);
        assert_eq!(app.queue_place(0), Some(QueuePlace::Waiting(0)));

        focus_on(&mut app, "title");
        press(&mut app, KeyCode::Enter);
        for c in "new".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);

        let atoms = app.queued_atoms_for_test(0).expect("still queued");
        assert!(atoms.iter().any(|(k, v)| k == "title" && v == "Anew"), "{atoms:?}");
        assert!(atoms.iter().any(|(k, v)| k == "channel" && v == "first"), "{atoms:?}");
        assert_eq!(app.status, "saved to the queued write");
        assert!(app.staged[&0].contains_key("title"), "the edit stays staged until it lands");
    }

    /// Undoing the only edit a queued file had takes the file off the queue:
    /// there is nothing left to write to it.
    #[test]
    fn undoing_a_queued_files_edits_removes_its_job() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("edited"));
        app.enqueue_for_test(0, false);
        app.undo();
        assert_eq!(app.queue_place(0), None);
    }

    /// The one file an edit cannot reach is the one under the writer. The
    /// edit stays staged, the plan is left alone, and the status says to
    /// press w again.
    #[test]
    fn an_edit_to_the_file_being_written_waits_for_the_next_w() {
        let mut app = pair();
        app.cycle_file(1);
        app.enqueue_for_test(0, true);
        assert_eq!(app.queue_place(0), Some(QueuePlace::Busy));

        focus_on(&mut app, "title");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Enter);

        assert!(app.status.contains("being written now") && app.status.contains("press w"), "{}", app.status);
        assert_eq!(app.staged[&0].get("title"), Some(&Value::text("Ax")));
    }

    /// Files ahead in the queue are counted from the one under the writer.
    #[test]
    fn queue_place_counts_the_files_written_before_this_one() {
        let mut app = pair();
        app.set_staged(0, "title", Value::text("x"));
        app.set_staged(1, "title", Value::text("y"));
        app.enqueue_for_test(0, false);
        app.enqueue_for_test(1, false);
        assert_eq!(app.queue_place(1), Some(QueuePlace::Waiting(1)));
    }

    /// A second w while a writer is running appends to its queue rather than
    /// starting a second writer, and says so.
    #[test]
    fn a_write_during_a_write_joins_the_queue() {
        let mut app = pair();
        app.enqueue_for_test(1, true); // a writer is alive, on b.mov
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("edited"));
        app.prepare_write();
        assert!(app.pending.is_some(), "{}", app.status);
        app.apply();
        assert_eq!(app.queue_place(0), Some(QueuePlace::Waiting(1)));
        assert!(app.status.contains("behind the running write"), "{}", app.status);
        assert!(app.writing);
    }

    /// `r` on a file with edits pending does not refuse and does not rename
    /// from the old tags: it flags the rename onto the write, the queued job
    /// carries the flag, and `r` again takes it off.
    #[test]
    fn a_rename_with_edits_pending_is_queued_onto_the_write() {
        let mut app = pair();
        app.cycle_file(1); // a.mov
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("edited"));
        app.enqueue_for_test(0, false);

        app.rename_files();
        assert!(app.rename_after.contains(&0));
        assert!(!app.renaming, "nothing renamed now");
        assert!(app.status.contains("rename queued"), "{}", app.status);
        assert_eq!(app.queued_rename_for_test(0), Some(true));

        // A later edit to the same file keeps the flag on the rebuilt job.
        app.stage("channel".into(), Value::text("c"));
        assert_eq!(app.queued_rename_for_test(0), Some(true));

        app.rename_files();
        assert!(!app.rename_after.contains(&0));
        assert_eq!(app.queued_rename_for_test(0), Some(false));
        assert!(app.status.contains("unqueued"), "{}", app.status);
    }

    /// A staged-but-not-yet-queued file takes the flag too, and `w` hands it
    /// to the job.
    #[test]
    fn the_flag_rides_into_the_job_on_w() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("edited"));
        app.rename_files();
        assert!(app.rename_after.contains(&0));
        app.enqueue_for_test(1, true); // a writer is alive, so apply only queues
        app.prepare_write();
        app.apply();
        assert_eq!(app.queued_rename_for_test(0), Some(true));
    }

    /// When the rename that followed a write comes home, the file's path
    /// moves with it and the flag is spent -- whatever the outcome.
    #[test]
    fn a_finished_rename_moves_the_path_and_clears_the_flag() {
        let mut app = pair();
        app.rename_after.insert(0);
        app.file_written(0, None, Ok(()), Some(Ok(Outcome::Renamed(PathBuf::from("/nonexistent/new.mov")))));
        assert!(app.files[0].path.ends_with("new.mov"));
        assert!(!app.rename_after.contains(&0));

        app.rename_after.insert(1);
        app.file_written(1, None, Ok(()), Some(Ok(Outcome::Taken(PathBuf::from("/x.mov")))));
        assert!(app.files[1].path.ends_with("b.mov"));
        assert!(!app.rename_after.contains(&1));
    }

    /// Written but not renamed is reported, and earns the dialog.
    #[test]
    fn a_rename_that_did_not_happen_is_reported() {
        let mut app = one(&[]);
        app.finish_write(WriteResults {
            verb: "Wrote",
            ok: vec![PathBuf::from("/x.mov")],
            not_renamed: vec![(PathBuf::from("/x.mov"), "name taken by y.mov".into())],
            ..Default::default()
        });
        assert!(app.status.contains("1 not renamed"), "{}", app.status);
        assert!(app.results.is_some());
    }

    /// Keys stay live during a write -- that is the point of the queue --
    /// but quitting is refused until it lands.
    #[test]
    fn quitting_waits_for_the_write() {
        let mut app = pair();
        app.enqueue_for_test(0, true);
        app.writing = true;
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        assert!(app.status.contains("write in progress"), "{}", app.status);
    }

    /// A clean run reports in the status line and gets out of the way; only
    /// a failure earns the dialog.
    #[test]
    fn only_a_failed_run_raises_the_results_dialog() {
        let mut app = one(&[]);
        app.finish_write(WriteResults {
            verb: "Wrote",
            ok: vec![PathBuf::from("/x.mov")],
            ..Default::default()
        });
        assert!(app.results.is_none());
        assert_eq!(app.status, "wrote 1 of 1");
        app.finish_write(WriteResults {
            verb: "Wrote",
            ok: vec![],
            failed: vec![(PathBuf::from("/x.mov"), "boom".into())],
            ..Default::default()
        });
        assert!(app.results.is_some());
    }

    /// `w` writes every staged edit, including one made on a file that is no
    /// longer in view -- otherwise the plan silently drops half the batch.
    #[test]
    fn the_plan_covers_files_that_are_not_in_view() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.stage("title".into(), Value::text("edited"));
        app.cycle_file(1);
        app.prepare_write();

        let plans = app.pending.as_ref().expect("a plan");
        assert_eq!(plans.len(), 1);
        assert!(plans[0].path.ends_with("a.mov"), "{:?}", plans[0].path);
    }

    /// An untouched control commits nothing. This is the rule the lost edits
    /// were breaking: tabbing through a form must not stage or unstage.
    #[test]
    fn moving_through_the_form_stages_nothing() {
        let mut app = pair();
        for _ in 0..app.rows.len() * 2 {
            app.move_focus(1);
        }
        assert!(app.staged.is_empty(), "{:?}", app.staged);
    }

    #[test]
    fn overwrite_all_puts_the_focused_value_on_every_file() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.copy_out(false);

        assert_eq!(app.staged[&1].get("title"), Some(&Value::text("A")));
        // Nothing staged on the file it came from: it already holds the value,
        // and an edit that changes nothing is not an edit.
        assert!(!app.staged.contains_key(&0), "{:?}", app.staged);
        assert!(app.status.contains("overwritten on 2 files"), "{}", app.status);
    }

    /// Backfill fills the gaps and disturbs nothing else.
    #[test]
    fn backfill_only_reaches_files_where_the_field_is_empty() {
        use crate::tags::probe::FileTags;
        let mut app = pair();
        app.files.push(FileTags {
            path: PathBuf::from("/nonexistent/c.mov"),
            atoms: BTreeMap::new(),
            xmp: BTreeMap::new(),
        });
        app.view = Some(0);
        app.rebuild_rows();
        focus_on(&mut app, "title");
        app.copy_out(true);

        assert_eq!(app.staged[&2].get("title"), Some(&Value::text("A")));
        assert!(!app.staged.contains_key(&1), "B kept its own title: {:?}", app.staged);
        assert!(app.status.contains("backfilled into 1 file"), "{}", app.status);
    }

    /// A clear reads as absent rather than as an empty string, so the row
    /// shows what the write will leave behind.
    #[test]
    fn a_cleared_field_shows_as_absent_and_stays_staged() {
        let mut app = pair();
        app.cycle_file(1);
        focus_on(&mut app, "title");
        app.clear_focused();
        assert_eq!(app.staged[&0].get("title"), Some(&Value::text("")));
        assert_eq!(row(&app, "title").shown(), None);
        assert!(row(&app, "title").staged);

        app.move_focus(1);
        app.move_focus(-1);
        assert_eq!(app.staged[&0].get("title"), Some(&Value::text("")));
    }
}

#[cfg(test)]
mod mixed_set_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Three files, two of them Clip and one Original: a mixed Variant.
    fn trio() -> App {
        use crate::tags::probe::FileTags;
        let mk = |name: &str, variant: &str| FileTags {
            path: PathBuf::from(format!("/nonexistent/{name}.mov")),
            atoms: BTreeMap::from([("variant".to_string(), Value::text(variant))]),
            xmp: BTreeMap::new(),
        };
        let mut app = App::new(
            vec![mk("a", "Clip"), mk("b", "Original"), mk("c", "Clip")],
            BTreeMap::new(),
            false,
        );
        app.focus = app.rows.iter().position(|r| r.key == "variant").unwrap();
        app
    }

    fn variant(app: &App) -> &Row {
        app.rows.iter().find(|r| r.key == "variant").unwrap()
    }

    /// Either direction settles on the answer most files already hold, not
    /// on the first or last option in the list.
    #[test]
    fn a_step_on_a_mixed_set_puts_every_file_on_the_commonest_answer() {
        for key in [KeyCode::Char('l'), KeyCode::Char('h'), KeyCode::Right, KeyCode::Left] {
            let mut app = trio();
            assert!(variant(&app).is_mixed());
            press(&mut app, key);
            assert_eq!(variant(&app).shown(), Some(&Value::text("Clip")), "{key:?}");
            // Only the file that disagreed carries an edit.
            assert_eq!(app.staged_count(), 1, "{key:?}");
        }
    }

    /// Consolidated, the set steps as any agreed set does.
    #[test]
    fn a_second_step_moves_off_the_consolidated_answer() {
        let mut app = trio();
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(variant(&app).shown(), Some(&Value::text("Original")));
    }

    /// Esc on the field puts every file back on its own answer: mixed again,
    /// nothing staged, and the quit prompt not raised.
    #[test]
    fn esc_on_a_consolidated_set_puts_it_back_to_mixed() {
        let mut app = trio();
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Esc);
        assert!(variant(&app).is_mixed());
        assert!(app.staged.is_empty(), "{:?}", app.staged);
        assert!(!app.quit && !app.confirm_quit);
        // And it is an ordinary edit: undo brings the consolidation back.
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(variant(&app).shown(), Some(&Value::text("Original")));
    }

    /// With nothing staged on the set, Esc means what it always meant.
    #[test]
    fn esc_on_an_untouched_set_still_quits() {
        let mut app = trio();
        press(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }

    #[test]
    fn a_tie_goes_to_the_earlier_option() {
        let opts: Vec<Opt> = ["Original", "Enhanced", "Clip"]
            .iter()
            .map(|s| Opt { code: s.to_string(), label: s.to_string() })
            .collect();
        let v = |s: &str| Some(Value::text(s));
        assert_eq!(majority(&[v("Clip"), v("Original")], &opts).as_deref(), Some("Original"));
        assert_eq!(majority(&[v("Clip"), v("Clip"), v("Original")], &opts).as_deref(), Some("Clip"));
        assert_eq!(majority(&[None, v("")], &opts), None);
    }
}
