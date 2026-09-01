//! Application state and the event loop (DESIGN §6.3).
//!
//! Milestone 2 is read-only: the form displays and navigates, nothing is
//! edited and nothing is written. The focus ring, the aggregate/single-file
//! split and the inspector are all here because they are what the editing
//! milestones plug into.

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::config::{Enums, KINDS};
use crate::model::schema::{Control, FieldDef, FIELDS};
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
                let mut out = String::with_capacity(lower.len());
                for word in lower.split_inclusive(char::is_whitespace) {
                    out.push_str(&upper_first(word));
                }
                out
            }
        }
    }
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

pub struct WriteResults {
    pub ok: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
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
    pub name: String,
    pub label: &'static str,
    /// Fraction of this file's work, 0..1.
    pub frac: f64,
    pub started: Instant,
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

pub enum Msg {
    Thumb(usize, Box<image::DynamicImage>),
    Media(usize, MediaInfo),
    /// A stage of the running write, from the writer thread.
    Progress(Box<WriteProgress>),
    Wrote(Box<WriteResults>),
    /// One outcome per file a `rename-video` run was given, by file index.
    Renamed(Vec<(usize, Result<Outcome, String>)>),
}

/// Staged edits, keyed by file index then by row key.
pub type Staged = BTreeMap<usize, BTreeMap<String, Value>>;

pub struct App {
    pub files: Vec<FileTags>,
    pub media: Vec<MediaInfo>,
    pub rows: Vec<Row>,
    /// How many trailing rows are unrecognised keys rather than schema fields.
    pub n_custom: usize,
    /// Keys no field claims, kept as names so their aggregate can be recomputed
    /// for whichever files are in scope -- otherwise a custom key would still
    /// read ‹multiple› while looking at a single file.
    custom_keys: Vec<String>,
    pub focus: usize,
    /// None = aggregate view over every file; Some(i) = that one file.
    pub view: Option<usize>,
    pub inspector: bool,
    pub status: String,
    pub enums: Enums,
    /// Ride the faststart flag along on any remux we are already doing. On by
    /// default, per the brief.
    pub faststart: bool,
    /// The write plan, awaiting confirmation. Nothing reaches disk until this
    /// has been shown and accepted.
    pub pending: Option<Vec<FilePlan>>,
    /// The outcome of the last write, held until dismissed.
    pub results: Option<WriteResults>,
    pub writing: bool,
    /// A `rename-video` run is in flight. It shells out to ffprobe and exiftool
    /// per file, so it runs off the UI thread like every other probe here --
    /// and while it does, the paths in `files` are the ones about to change,
    /// which is why `w` and a second `r` are held off until it lands.
    pub renaming: bool,
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
        let n_custom = custom_keys.len();
        let scope: Vec<usize> = (0..files.len()).collect();
        let rows = build_rows(&files, &scope, &Staged::new(), &custom_keys);
        let (tx, rx) = mpsc::channel();
        let n = files.len();
        let mut app = Self {
            media: vec![MediaInfo::default(); n],
            files,
            rows,
            n_custom,
            custom_keys,
            focus: 0,
            view: None,
            inspector: false,
            status: String::new(),
            enums: Enums::load(),
            faststart: true,
            pending: None,
            results: None,
            writing: false,
            renaming: false,
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
        self.thumb_aspect = None;
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
                    if i < self.media.len() {
                        self.media[i] = info;
                    }
                }
                Msg::Progress(p) => {
                    if self.writing {
                        self.progress = Some(*p);
                    }
                }
                Msg::Wrote(r) => self.finish_write(*r),
                Msg::Renamed(r) => self.finish_rename(r),
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
                }
            })
            .collect()
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
        if self.staged.is_empty() {
            self.status = "nothing to write".into();
            return;
        }
        // Every staged edit, not just the ones in view: an edit belongs to the
        // file it was made on, and silently skipping the file you are not
        // looking at is how a batch loses half its work.
        let plans: Vec<FilePlan> = self
            .staged
            .iter()
            .map(|(i, edits)| plan::build(&self.files[*i], edits, self.faststart))
            .filter(|p| !p.is_empty())
            .collect();
        if plans.is_empty() {
            self.status = "nothing to write".into();
            return;
        }
        self.pending = Some(plans);
    }

    /// Start the confirmed plan on its own thread, reporting progress back
    /// through the same channel the thumbnails use.
    ///
    /// Off-thread because a remux is minutes of work and the event loop must
    /// keep painting: the bar is the whole point, and it cannot move from
    /// inside a blocking call.
    fn apply(&mut self) {
        let Some(plans) = self.pending.take() else { return };
        let jobs: Vec<(FilePlan, BTreeMap<String, Value>)> = plans
            .into_iter()
            .map(|p| {
                let snapshot = self
                    .files
                    .iter()
                    .find(|f| f.path == p.path)
                    .map(|f| f.xmp.clone())
                    .unwrap_or_default();
                (p, snapshot)
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        self.writing = true;
        let started = Instant::now();
        self.progress = Some(WriteProgress {
            file: 0,
            total: jobs.len(),
            name: file_name(&jobs[0].0.path),
            label: "starting",
            frac: 0.0,
            started,
        });
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let total = jobs.len();
            let mut written: Vec<PathBuf> = Vec::new();
            let mut failed: Vec<(PathBuf, String)> = Vec::new();
            for (i, (plan, snapshot)) in jobs.iter().enumerate() {
                let name = file_name(&plan.path);
                let tick = &tx;
                let mut on = |s: write::Step| {
                    let _ = tick.send(Msg::Progress(Box::new(WriteProgress {
                        file: i,
                        total,
                        name: name.clone(),
                        label: s.label,
                        frac: s.frac,
                        started,
                    })));
                };
                match write::execute(plan, snapshot, &mut on) {
                    Ok(()) => written.push(plan.path.clone()),
                    // A bad file in a batch must not cost the others their write.
                    Err(e) => failed.push((plan.path.clone(), e.to_string())),
                }
            }
            let _ = tx.send(Msg::Wrote(Box::new(WriteResults { ok: written, failed })));
        });
    }

    /// The write is over: re-read from disk so the form shows what is actually
    /// on the files rather than what was hoped for.
    ///
    /// An edit is dropped only once the files agree with it. Clearing the whole
    /// staging map here cost a failed write everything that had been typed into
    /// it -- the form was the only place those edits existed, and the retry the
    /// error message invites began with retyping them.
    fn finish_write(&mut self, results: WriteResults) {
        for f in self.files.iter_mut() {
            if let Ok(fresh) = crate::tags::probe::probe(&f.path) {
                *f = fresh;
            }
        }
        // Compared against what is now on disk rather than against the list of
        // files that succeeded: in a mixed batch an edit can land on four files
        // and fail on the fifth, and it is still an edit until the fifth has it.
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
        self.rebuild_rows();
        self.writing = false;
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
        self.results = Some(results);
    }

    /// `r`: hand the files in scope to `rename-video`, which names each one
    /// from its own tags. Filename sync as designed (DESIGN §9.4) is not built
    /// here; the tool already composes both of this library's grammars, and one
    /// grammar in one place is the point.
    ///
    /// Disk tags, not staged ones: the tool re-probes each file, so a rename
    /// run before the write would build the name out of the values the edit is
    /// about to replace. Refusing is better than a name that is stale the
    /// moment `w` lands.
    fn rename_files(&mut self) {
        self.commit_editor();
        if self.renaming {
            return;
        }
        let scope = self.scope();
        let stale = scope
            .iter()
            .any(|i| self.staged.get(i).is_some_and(|e| !e.is_empty()));
        if stale {
            self.status = "staged edits: write with w first, then rename".into();
            return;
        }
        let jobs: Vec<(usize, PathBuf)> =
            scope.iter().map(|i| (*i, self.files[*i].path.clone())).collect();
        let Some((_, first)) = jobs.first() else { return };
        self.status = match jobs.len() {
            1 => format!("renaming {}", file_name(first)),
            n => format!("renaming {n} files"),
        };
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
        // Why a file kept its name, kept separately from the names that changed:
        // in a mixed batch the interesting half is the half that did not move,
        // and the strip has room for one line.
        let mut note = String::new();
        for (i, r) in out {
            match r {
                Ok(Outcome::Renamed(to)) => {
                    name = file_name(&to);
                    if let Some(f) = self.files.get_mut(i) {
                        f.path = to;
                    }
                    renamed += 1;
                }
                Ok(Outcome::Unchanged) => note = "already named from its tags".into(),
                Ok(Outcome::Taken(to)) => note = format!("name taken: {}", file_name(&to)),
                Err(e) => note = e,
            }
        }
        self.status = match (renamed, total) {
            (0, _) => note,
            (1, 1) => format!("renamed to {name}"),
            (n, t) if n == t => format!("renamed {n} files"),
            (n, t) => format!("renamed {n} of {t}: {note}"),
        };
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
        // Nothing to press while a write runs: the files are being replaced
        // under us, and a stray key must not stage an edit against them.
        if self.writing {
            return;
        }
        if self.results.is_some() {
            self.results = None;
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
            KeyCode::Enter => {
                self.commit_editor();
                self.mode = Mode::Select;
                self.status.clear();
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
        match (key.code, ctrl) {
            (KeyCode::Char('j'), false) | (KeyCode::Down, _) | (KeyCode::Tab, _) => {
                self.move_focus(1)
            }
            (KeyCode::Char('k'), false) | (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
                self.move_focus(-1)
            }
            (KeyCode::Char('h'), false) | (KeyCode::Left, _) => self.nudge(-1),
            (KeyCode::Char('l'), false) | (KeyCode::Right, _) => self.nudge(1),
            (KeyCode::Char('g'), false) => self.jump(0),
            (KeyCode::Char('G'), false) => self.jump(self.rows.len().saturating_sub(1)),
            (KeyCode::Enter, _) => self.begin_edit(),
            (KeyCode::Char('i'), false) => {
                self.inspector = !self.inspector;
                self.status.clear();
            }
            (KeyCode::Char('y'), false) => self.yank(),
            (KeyCode::Char('p'), false) => self.paste(),
            (KeyCode::Char(']'), false) => self.cycle_file(1),
            (KeyCode::Char('['), false) => self.cycle_file(-1),
            (KeyCode::Char('a'), false) => {
                self.commit_editor();
                self.view = None;
                self.rebuild_rows();
                self.status = "aggregate view".into();
            }
            (KeyCode::Char('m'), false) => self.merge_focused(),
            (KeyCode::Char('o'), false) => self.copy_out(false),
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
            (KeyCode::Char('F'), false) => {
                self.faststart = !self.faststart;
                self.status = format!("faststart {}", if self.faststart { "on" } else { "off" });
            }
            (KeyCode::Char('q'), false) | (KeyCode::Esc, _) => self.escape(),
            _ => {}
        }
    }

    fn jump(&mut self, to: usize) {
        self.commit_editor();
        self.focus = to.min(self.rows.len().saturating_sub(1));
        self.open_editor();
    }

    fn begin_edit(&mut self) {
        match self.rows.get(self.focus) {
            Some(row) if !row.editable() => {
                self.status = format!("{} is read-only", row.label);
            }
            Some(_) => {
                self.open_editor();
                if let Some(ed) = &mut self.editor {
                    ed.select_first_if_unset();
                }
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
        let Some(row) = self.rows.iter().find(|r| r.key == key) else { return 0 };
        let (control, opts) = (row.control, self.options_for(row));
        let before = self.staged.clone();
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
        if before != self.staged {
            self.undo.push(before);
            self.redo.clear();
        }
        n
    }

    /// Push the focused field out to every open file — over whatever they
    /// hold (`o`, overwrite all), or into only the ones where it is still
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
        self.stage(key, new);
    }

    pub fn validation(&self) -> Validation {
        self.editor.as_ref().map(|e| e.validate()).unwrap_or(Validation::Ok)
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

    /// Whether a given file carries an edit for a key — the inspector's
    /// question, since it lists the selection file by file.
    pub fn file_is_staged(&self, file: usize, key: &str) -> bool {
        self.staged.get(&file).is_some_and(|m| m.contains_key(key))
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.staged, prev));
            self.rebuild_rows();
            self.status = format!("undo · {} staged", self.staged_count());
        } else {
            self.status = "nothing to undo".into();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.staged, next));
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
        self.status = match self.view {
            Some(i) => format!("file {} of {}", i + 1, self.files.len()),
            None => "aggregate view".into(),
        };
    }
}

/// Visible rows: every primary field, plus footage fields only once they hold
/// something. An absent primary field still gets a row -- seeing that Title is
/// empty is the point of a form.
/// What one file holds for a row key, whether the key is a schema field or an
/// unclaimed atom or XMP tag carried through from disk.
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

fn build_rows(
    files: &[FileTags],
    scope: &[usize],
    staged: &Staged,
    custom_keys: &[String],
) -> Vec<Row> {
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
            Some(row(def.id.to_string(), def.label.to_string(), def.control, Some(def), disk))
        })
        .collect();
    // Keys are already named by origin ("custom:" atom / "xmp:" tag), which the
    // write plan needs in order to put an edit back where it came from.
    rows.extend(custom_keys.iter().map(|k| {
        let disk = scope.iter().map(|i| disk_value(&files[*i], k)).collect();
        row(k.clone(), custom_label(k), Control::Text, None, disk)
    }));
    rows
}

/// One staged edit as the confirmation dialog needs it.
pub struct StagedEdit {
    pub label: String,
    pub shown: String,
    pub files: usize,
    /// Distinct values on disk this edit is about to replace.
    pub overwrites: usize,
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
    let picker = make_picker(no_thumbnail);

    let mut app = App::new(files, custom, !no_thumbnail);
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
        WriteProgress { file, total, name: String::new(), label: "", frac, started: Instant::now() }
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
            ok: vec![],
            failed: vec![(PathBuf::from("/nonexistent/tagform-test.mov"), "boom".into())],
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
            ok: vec![PathBuf::from("/nonexistent/tagform-test.mov")],
            failed: vec![],
        });

        assert!(app.staged.is_empty(), "{:?}", app.staged);
        assert_eq!(app.status, "wrote 1 of 1");
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
