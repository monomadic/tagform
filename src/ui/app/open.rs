//! The Open menu: `o`, then what to hand the desktop -- the file, its
//! folder, the URL, a Finder reveal, or the coordinates as a map pin.
//!
//! A child of `app` for the same reason as `import`.

use super::*;

/// What `o` can hand to the desktop. The menu keeps a cursor on one of
/// these; the order here is the order it paints and the cursor walks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenTarget {
    /// The file, in whatever the desktop plays it with.
    Default,
    /// The folder the file is in.
    Parent,
    /// The URL field, in the browser.
    Url,
    /// The file, selected in a Finder window.
    Reveal,
    /// The Coordinates field, as a pin in Apple Maps.
    AppleMaps,
    /// The Coordinates field, as a pin in Google Maps.
    GoogleMaps,
}

impl OpenTarget {
    pub const ALL: [OpenTarget; 6] = [
        OpenTarget::Default,
        OpenTarget::Parent,
        OpenTarget::Url,
        OpenTarget::Reveal,
        OpenTarget::AppleMaps,
        OpenTarget::GoogleMaps,
    ];

    /// Whether the choice needs the file's coordinates to do anything.
    pub fn needs_coords(self) -> bool {
        matches!(self, OpenTarget::AppleMaps | OpenTarget::GoogleMaps)
    }

    pub fn label(self) -> &'static str {
        match self {
            OpenTarget::Default => "Open with the default application.",
            OpenTarget::Parent => "Open parent directory.",
            OpenTarget::Url => "Open URL in the default browser.",
            OpenTarget::Reveal => "Reveal the file in Finder.",
            OpenTarget::AppleMaps => "Open coordinates in Apple Maps.",
            OpenTarget::GoogleMaps => "Open coordinates in Google Maps.",
        }
    }
}

impl App {
    /// The one file `o` acts on. The aggregate view has no one file to open,
    /// so it names the key that picks one rather than opening an arbitrary
    /// member of the selection.
    pub(super) fn open_subject(&self) -> Option<usize> {
        match self.view {
            Some(i) => Some(i),
            None if self.files.len() == 1 => Some(0),
            None => None,
        }
    }

    /// `o`: the menu of ways to hand the file to the desktop. Tagging is a
    /// claim about what a file holds, and the one check the form cannot make
    /// is whether the footage is the footage you think it is.
    pub(super) fn open_open(&mut self) {
        self.commit_editor();
        if self.open_subject().is_none() {
            self.status = "no single file in view -- pick one with ] first".into();
            return;
        }
        self.open_menu = Some(OpenTarget::Default);
        self.status.clear();
    }

    /// Walk the cursor, wrapping the way the form's own j/k do.
    pub(super) fn move_open(&mut self, at: OpenTarget, delta: isize) {
        let all = OpenTarget::ALL;
        let n = all.len() as isize;
        let i = all.iter().position(|t| *t == at).unwrap_or(0) as isize;
        self.open_menu = Some(all[(((i + delta) % n + n) % n) as usize]);
    }

    /// Run one choice from the menu through the desktop's launcher -- `open`
    /// on macOS, `xdg-open` elsewhere.
    ///
    /// Spawned and never waited on: the launcher returns long before the
    /// player does, and this thread owes the event loop a repaint in the
    /// meantime.
    pub(super) fn open_target(&mut self, target: OpenTarget) {
        let Some(idx) = self.open_subject() else {
            return;
        };
        let path = self.files[idx].path.clone();
        // A bare filename has an empty parent, which is the directory we
        // are in.
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let tool = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let mut cmd = std::process::Command::new(tool);
        let done = match target {
            OpenTarget::Default => {
                cmd.arg("--").arg(&path);
                format!("opened {}", file_name(&path))
            }
            OpenTarget::Parent => {
                cmd.arg("--").arg(&parent);
                format!("opened {}", parent.display())
            }
            OpenTarget::Url => {
                let Some(url) = self.url_of(idx) else {
                    self.status = "no URL on this file".into();
                    return;
                };
                // The launcher opens anything it is given, and the field is
                // free text: only a web address goes to it, so a path typed
                // there cannot launch whatever it names.
                let lower = url.to_ascii_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                    self.status = format!("not a web address: {url}");
                    self.status_error = true;
                    return;
                }
                cmd.arg("--").arg(&url);
                format!("opened {url}")
            }
            // Only Finder can select a file in its folder; elsewhere the
            // folder itself is as near as the launcher gets.
            OpenTarget::Reveal if cfg!(target_os = "macos") => {
                cmd.arg("-R").arg("--").arg(&path);
                format!("revealed {}", file_name(&path))
            }
            OpenTarget::Reveal => {
                cmd.arg("--").arg(&parent);
                format!("opened {}", parent.display())
            }
            // The coordinates as a pin. Both URLs are the services' own
            // documented forms; on macOS the launcher hands maps.apple.com
            // to Maps itself, and the browser gets the other.
            OpenTarget::AppleMaps | OpenTarget::GoogleMaps => {
                let Some((lat, lon)) = self.coords_of(idx) else {
                    self.status = "no coordinates on this file".into();
                    return;
                };
                let url = if target == OpenTarget::AppleMaps {
                    format!("https://maps.apple.com/?ll={lat},{lon}&q={lat},{lon}")
                } else {
                    format!("https://www.google.com/maps/search/?api=1&query={lat},{lon}")
                };
                cmd.arg("--").arg(&url);
                format!(
                    "opened {lat},{lon} in {}",
                    if target == OpenTarget::AppleMaps {
                        "Apple Maps"
                    } else {
                        "Google Maps"
                    }
                )
            }
        };
        let spawned = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = match spawned {
            Ok(_) => done,
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
    pub(super) fn copy_out(&mut self, only_empty: bool) {
        let Some(row) = self.rows.get(self.focus) else {
            return;
        };
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
            (0, false) => "nothing to copy into".to_string(),
            (_, true) => format!("{label} backfilled into {files}"),
            (_, false) => format!("{label} overwritten on {files}"),
        };
    }
}
