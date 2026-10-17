//! The import band: `i`, then a source -- the page behind the URL field
//! (`u`, yt-dlp), the filename (`f`, the two grammars), or a place looked up
//! (`l`, MapKit, §5.5). Also the seed on open, which is the filename import
//! run without asking. Everything here stages; nothing writes.
//!
//! A child of `app` so it can reach the form's private state; the methods are
//! `pub(super)` for the key router and `pub` where the painter reads them.

use super::*;

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
    pub const ALL: [ImportSource; 3] = [
        ImportSource::Url,
        ImportSource::Filename,
        ImportSource::Location,
    ];
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
    Pick {
        hits: Vec<Hit>,
        at: usize,
        named: bool,
    },
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

impl App {
    /// On open: read the fields a structured filename carries into the fields
    /// the file leaves empty, the same import `i f` runs, staged and undoable
    /// in one step. Only a name with structure -- people, a channel, tags,
    /// stars, a date -- is read. A bare stem is all title to the parser, and
    /// `IMG_0412` or `clip-3` is not a title anyone chose.
    pub fn seed_from_filenames(&mut self) {
        let out: fetch::Fetched = (0..self.files.len())
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
            let from = if self.files.len() == 1 {
                "the filename"
            } else {
                "the filenames"
            };
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
    pub(super) fn fetch_tags(&mut self) {
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
        let cache = self.fetch_cache.clone();
        std::thread::spawn(move || {
            let out = jobs
                .iter()
                // One page down must not cost the rest of the batch its tags.
                // Through the cache, so the second file to name a page gets
                // the first one's answer: sequential for exactly that reason.
                .map(|(i, url)| (*i, cache.fetch(url).map_err(|e| format!("{e:#}"))))
                .collect();
            let _ = tx.send(Msg::Fetched(out));
        });
    }

    /// Stage what the pages said, as one undoable step. A field a page did not
    /// answer is not in its list and so is not touched -- the existing value,
    /// staged or on disk, stays. A field it did answer is replaced: the point
    /// of pressing `i u` is to take the page's word for it, and `u` takes it
    /// back in one key if the page was wrong.
    pub(super) fn finish_fetch(&mut self, out: fetch::Fetched) {
        self.fetching = false;
        self.stage_import("fetched", "the page agrees with the file", out, false);
    }

    /// `i` then `f`: read the fields the filename carries (§9.4) and stage
    /// them into the fields that are still empty. Only the empty ones, unlike
    /// the fetch: a page is an authority worth taking the word of, but a name
    /// was composed *from* tags, so where the container disagrees with it the
    /// container is the newer of the two. Per file in scope, each from its
    /// own name -- plus the one field that is read from the batch rather than
    /// from a name, `filename_tracks` below. Synchronous -- there is nothing
    /// to wait on.
    pub(super) fn import_filename(&mut self) {
        self.commit_editor();
        let tracks = self.filename_tracks();
        let out = self
            .scope()
            .into_iter()
            .map(|i| {
                let mut fields = filename::parse_path(&self.files[i].path);
                if let Some(n) = tracks.get(&i) {
                    fields.push(("track", Value::Text(n.to_string())));
                }
                let r = if fields.is_empty() {
                    Err(format!(
                        "nothing recognised in {}",
                        file_name(&self.files[i].path)
                    ))
                } else {
                    Ok(fields)
                };
                (i, r)
            })
            .collect();
        self.stage_import(
            "imported",
            "every field the name carries is already set",
            out,
            true,
        );
    }

    /// Stage what a source said about each file, as one undoable step, and
    /// say what happened. A field a source did not answer is not in its list
    /// and so is not touched. With `only_empty`, a field that already shows
    /// a value keeps it -- the filename rule; without, the source's value
    /// replaces it -- the fetch's rule, where `u` takes the whole step back.
    pub(super) fn stage_import(
        &mut self,
        verb: &str,
        agrees: &str,
        out: fetch::Fetched,
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
            self.evict_edited(&touched);
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
            (n, t) => format!(
                "{verb} {} across {n} of {t} files: {note}",
                n_fields(filled)
            ),
        };
    }

    /// The Track number each file's name gives it, when the batch in scope is
    /// a set of clips numbered through their names (§9.4).
    ///
    /// Three conditions, all of them narrow on purpose. More than one file:
    /// a sequence needs something to climb against, and a single file in view
    /// has no batch to read. Every file in scope agreeing that its Variant is
    /// a Clip: that is the one Variant whose number means "cut N of a work",
    /// and the only one the form offers a Track row for unprompted. And a set
    /// of names that spells out exactly one climbing number -- ambiguity is
    /// `filename::track_sequence`'s to refuse, and it does.
    ///
    /// What comes back is only what the names say. The import stages it like
    /// every other field a name carries, which is to say into the empty ones
    /// only: a Track already on a file outranks a guess made from a filename.
    pub(super) fn filename_tracks(&self) -> BTreeMap<usize, u32> {
        let scope = self.scope();
        if scope.len() < 2 {
            return BTreeMap::new();
        }
        let clip = agreed_field(&self.files, &scope, &self.staged, "variant")
            .is_some_and(|v| v.eq_ignore_ascii_case(CLIP));
        if !clip {
            return BTreeMap::new();
        }
        let stems: Vec<String> = scope
            .iter()
            .map(|i| {
                self.files[*i]
                    .path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let stems: Vec<&str> = stems.iter().map(String::as_str).collect();
        match filename::track_sequence(&stems) {
            Some(ns) => scope.into_iter().zip(ns).collect(),
            None => BTreeMap::new(),
        }
    }

    /// What the import menu shows beside each source, for the file in view
    /// (the first in scope, in the aggregate). Computed here rather than in
    /// the painter so the preview and the import cannot disagree about what
    /// the name says.
    /// The URL field of one file as the form shows it -- the staged edit
    /// where there is one, else disk -- if it holds anything.
    pub fn url_of(&self, idx: usize) -> Option<String> {
        let f = self.files.get(idx)?;
        let disk = disk_value(f, "url");
        match overlay(disk, self.staged.get(&idx).and_then(|m| m.get("url")))? {
            Value::Text(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => None,
        }
    }

    pub fn import_preview(&self) -> ImportPreview {
        let idx = self.current_file();
        let scope = self.scope();
        let url = self.url_of(idx);
        let path = self
            .files
            .get(idx)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut fields = filename::parse_path(&path);
        if let Some(n) = self.filename_tracks().get(&idx) {
            fields.push(("track", Value::Text(n.to_string())));
        }
        let mut fills = Vec::new();
        let mut keeps = Vec::new();
        for (id, value) in fields {
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
        ImportPreview {
            files: scope.len(),
            url,
            stem,
            fills,
            keeps,
            place,
            coords: self.coords_of(idx),
        }
    }

    /// The location block as the form shows it for one file, joined into the
    /// query a lookup would start from: "Coro Hotel, Makati, Metro Manila".
    pub(super) fn place_text(&self, idx: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        for id in [
            "location_place",
            "location",
            "location_state",
            "location_country",
        ] {
            let disk = self.files.get(idx).and_then(|f| disk_value(f, id));
            if let Some(Value::Text(s)) =
                overlay(disk, self.staged.get(&idx).and_then(|m| m.get(id)))
            {
                let s = s.trim();
                if !s.is_empty() && !parts.iter().any(|p| p == s) {
                    parts.push(s.to_string());
                }
            }
        }
        parts.join(", ")
    }

    /// The text the form shows for one file's row, staged edit over disk,
    /// if there is any.
    pub(super) fn shown_text(&self, idx: usize, key: &str) -> Option<String> {
        let disk = self.files.get(idx).and_then(|f| disk_value(f, key));
        match overlay(disk, self.staged.get(&idx).and_then(|m| m.get(key)))? {
            Value::Text(s) if !s.trim().is_empty() => Some(s),
            _ => None,
        }
    }

    /// The coordinates the form shows for one file, if they parse.
    pub fn coords_of(&self, idx: usize) -> Option<(f64, f64)> {
        geocode::parse_iso6709(&self.shown_text(idx, "coordinates")?)
    }

    /// `i l`: open the lookup prompt over the location block as it stands.
    pub(super) fn open_locate(&mut self) {
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
    pub(super) fn run_locate(&mut self) {
        let Some(Locate::Ask(ed)) = &self.locate else {
            return;
        };
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
    pub(super) fn name_the_coordinates(&mut self) -> bool {
        let Some((lat, lon)) = self.coords_of(self.current_file()) else {
            return false;
        };
        self.status = format!("naming the place at {}", geocode::iso6709(lat, lon));
        self.status_error = false;
        self.spawn_lookup(move || geocode::reverse(lat, lon), false);
        true
    }

    /// Search for a typed place, from the Place row or the `i l` prompt.
    pub(super) fn start_lookup(&mut self, query: String) {
        self.status = format!("looking up {query}");
        self.status_error = false;
        self.spawn_lookup(move || geocode::search(&query), true);
    }

    pub(super) fn spawn_lookup(
        &mut self,
        job: impl FnOnce() -> anyhow::Result<Vec<Hit>> + Send + 'static,
        named: bool,
    ) {
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
    pub(super) fn finish_locate(&mut self, r: Result<Vec<Hit>, String>, named: bool) {
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
    pub(super) fn move_pick(&mut self, delta: isize) {
        if let Some(Locate::Pick { hits, at, .. }) = &mut self.locate {
            let n = hits.len() as isize;
            *at = (((*at as isize + delta) % n + n) % n) as usize;
        }
    }

    /// Stage one hit's location block onto every file in scope, as one
    /// undoable step, the way a fetch stages a page's answers.
    ///
    /// Two of the hit's parts defer to the file. A reverse hit names the
    /// venue only on a file whose Place is empty: the nearest thing MapKit
    /// knows is a guess, and a Place someone wrote is not. A typed lookup
    /// leaves Coordinates the camera filled alone: a place is a name for
    /// where the clip was shot, and the camera's fix is a measurement of
    /// it, which the geocoder's centre of a city would only coarsen.
    pub(super) fn stage_hit(&mut self, hit: Hit, named: bool) {
        self.locate = None;
        let out = self
            .scope()
            .into_iter()
            .map(|i| {
                let place_empty = self.shown_text(i, "location_place").is_none();
                let mut fields = hit.fields(named || place_empty);
                if named && self.coords_of(i).is_some() {
                    fields.retain(|(k, _)| *k != "coordinates");
                }
                (i, Ok(fields))
            })
            .collect();
        self.stage_import("located", "the place agrees with the file", out, false);
        if !self.status_error {
            self.status = format!("{}: {}", hit.summary(), self.status);
        }
    }

    /// `i`: open the menu, with the cursor on a source that has something to
    /// offer. A file with no URL cannot be fetched for, so opening on `url`
    /// there would put ⏎ on the one choice that refuses -- the cursor starts
    /// on the filename instead, and `u` still reaches the other one.
    pub(super) fn open_import(&mut self) {
        if self.import_preview().url.is_none() {
            self.import_pick = ImportSource::Filename;
        }
        self.import_menu = true;
        self.status.clear();
    }

    /// Walk the cursor, wrapping the way the form's own j/k do.
    pub(super) fn move_import(&mut self, delta: isize) {
        let all = ImportSource::ALL;
        let n = all.len() as isize;
        let at = all.iter().position(|s| *s == self.import_pick).unwrap_or(0) as isize;
        self.import_pick = all[(((at + delta) % n + n) % n) as usize];
    }

    /// Close the menu and run the chosen source.
    pub(super) fn run_import(&mut self, source: ImportSource) {
        self.import_menu = false;
        self.import_pick = source;
        match source {
            ImportSource::Url => self.fetch_tags(),
            ImportSource::Filename => self.import_filename(),
            ImportSource::Location => self.open_locate(),
        }
    }
}
