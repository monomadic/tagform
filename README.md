# tagform

A form-based metadata tagger for MP4/MOV — labelled fields with typed editors,
validation, enums, star rows and tag chips, instead of a list of key/value
strings. Replaces `mp4-tui-tagger`.

- **[DESIGN.md](DESIGN.md)** — the design. Written ahead of the code, so it marks
  what is not built (`⟨designed⟩`) and what shipped differently
  (`⟨built, differs⟩`). §16 has the current direction.
- **[docs/CONTAINER.md](docs/CONTAINER.md)** — what ffmpeg and exiftool
  *actually* write. Measured. Read this before changing the write path.
- **[AGENTS.md](AGENTS.md)** — orientation for coding agents.

## Status

**Milestones 0–5 done; 6 mostly done.** Probe → model → aggregate → typed
controls → verified write, across a whole selection. Edits stage until `w`,
which shows a plan to confirm; the original is only ever replaced by a result
that has been read back and checked.

XMP is read, written, and **restored across a remux** — which is the whole
reason this tool exists, since an ffmpeg remux destroys XMP silently and
`rename-footage` puts everything there. The writer picks one of three backends
from the file's contents and verifies the result before replacing anything.

Not built: the two filename grammars (the rest of milestone 6), seeding
(`--fetch`, `--from-filename`, completion), headless `--set`/`--apply`, and a
config file. DESIGN §16 says what is next and why the list is shorter than the
original plan.

Aggregation works like an mp3 tagger: a field that differs between files reads
`‹multiple›`, and is left alone unless you set it. `m` **merges** a list field —
the union of every file's values, first-seen order, folded case-insensitively —
which is the operation you actually want when tagging a batch and which none of
the scripts this replaces can do. Setting a `‹multiple›` field says how many
distinct values it is about to flatten, in the confirmation, before it happens.

```bash
cargo run -- FILE...                  # the form
cargo run -- --print-json FILE...     # the model, as JSON
cargo run -- --print-schema           # the field schema, as JSON
cargo test
cargo build --release                 # binary to target/release/tagform
```

The full CLI is five options: `--print-json`, `--print-schema`,
`--no-thumbnail`, `--theme=NAME`, `--help`. Everything else is a key inside the
form.

`--print-schema` is where the metadata vocabulary is documented — every field
with the keys it **writes** (`mdta`), the aliases it **understands** on read,
its XMP tag and its iTunes atom. It is emitted from `FIELDS` in
[src/model/schema.rs](src/model/schema.rs), which is the single authority; no
document keeps a copy.

The form is **modal**. Select mode moves and commands; Edit mode types. That is
what frees the single-letter keys — `w` can mean write because in Select mode
nothing is listening for the letter w.

**Select** (default)

| key | |
|---|---|
| `j` / `k`, arrows, `tab` | move between fields (`g` / `G` first / last) |
| `h` / `l` | cycle a fixed-set field, or nudge a rating, without entering edit mode |
| `enter` | edit the focused field |
| `w` | write staged edits (shows a plan first) |
| `m` | merge a list field across every file in the selection |
| `p` | inspector — per-file values for the focused field |
| `]` / `[` / `a` | next file / previous file / all files |
| `u` / `ctrl-r` / `r` | undo / redo / revert every staged edit |
| `backspace` | clear the focused field |
| `c` | cycle the colour scheme |
| `f` | toggle MOV faststart on the write (on by default) |
| `q` / `esc` | quit (asks if edits are staged) |

**Edit**

| key | |
|---|---|
| (type) | edit the field |
| `h` / `l`, `←` / `→` | step a fixed set, or adjust a rating |
| `enter` | save and stop editing |
| `tab` / `shift-tab` | save and move to the next / previous field |
| `j` / `k`, `↑` / `↓` | save and move a row — on a set, where there is no text to type |
| `esc` | cancel this field's edit |
| `ctrl-c` | quit, from either mode |

On a text field, the emacs/macOS editing keys work as they do everywhere else:

| key | |
|---|---|
| `ctrl-a` / `ctrl-e` | start / end of line |
| `ctrl-b` / `ctrl-f` | back / forward one character |
| `ctrl-d` / `ctrl-h` | delete the character right / left |
| `ctrl-w` | delete the word behind the cursor |
| `ctrl-k` | delete to end of line |
| `ctrl-u` | clear the line |

They bind only while a field is open, so Select mode's single-letter commands
are untouched — which is what the mode split is for. Every text-backed control
gets them, chips included, since a list is one joined line underneath.

The form paints its own chrome: a filled `tagform` badge heads the screen, every
field shows a coloured editable region whether or not it is focused, the focused
field is marked `▍` (`▶` while editing) and a staged one `●`, and a shortcut
strip along the bottom lists the keys that are live in the current mode.
Colours are true-colour throughout, in four schemes — `midnight`, `gruvbox`,
`nord`, `rose-pine` — cycled with `c` or picked with `--theme=NAME`. A test
computes WCAG contrast for every text colour in every scheme against that
scheme's own background and fails below 3:1, and checks that a custom-key label
is a different *hue* from an ordinary one rather than a dimmer shade. Both
guards exist because both mistakes were made: 16-colour `DarkGray` labels, and
a file path drawn in a divider colour at 1.4:1.

Controls: text, multi-line text, lists drawn as chips, `#hashtags`, URL
(validated, `not a URL: …`), dates, a 0–5 star row, fixed sets (Category,
Variant and Kind — the last stored as the `stik` integer but shown as "Movie"),
and read-only fields for things a camera wrote. Chips are a *rendering*: a list
edits as its comma-joined text and re-splits on commit, so there is no per-chip
cursor.

Twenty fields, in one flat list — no collapsed sections. The five footage
fields (Location, State, Country, Coordinates, Original name) appear only when
a file in the selection actually carries them. Anything on disk that no field
claims gets a row of its own at the bottom, atoms and XMP alike, so nothing is
lost by going unrecognised.

A set draws itself **along the field's own line** while that field is open,
with the current value lit — so you can see the whole set without cycling
blind, and opening one never changes the shape of the form:

```
   Variant          Clip
  ▶Variant           Clip  Enhanced  Original
```

`enter` opens, `h`/`l` step and wrap, `enter` accepts, `tab` accepts and
advances, `j`/`k` accept and move a row, `esc` reverts. Nothing is staged until
you accept, which is what makes `esc` clean. Opening an empty field lands on
the first option; until it is opened, it still reads as `—`.

Typing into a set is not supported yet — Category and Variant are picked from
the list, like Kind. A value already on the file that the list does not know
**joins the set for that field**, so an unfamiliar Category stays selectable
instead of being lost the first time the field is stepped.

Category and Variant are **not hardcoded** — they are parsed out of
`~/.config/yt-dlp/config`'s `--alias` lines, so adding an alias there adds a
dropdown value here.

**Category is what used to be called Genre**: `Media`, `Footage`, `Karaoke`,
`Live Visual` — what kind of thing the file is, which was never a style and was
sitting on the one key Plex, Jellyfin and Music.app read as one. Genre is still
there, on `genre`/`©gen`, now an ordinary text field holding the style those
players expect. Nothing migrates: the two keys are independent.

A value stored under an older name reads as its current one — `Camera Footage`
shows as `Footage`, `Master` as `Enhanced`, `VJ Clip` as `Live Visual` — on
files as well as in the dropdown, so a renamed option replaces the old spelling
instead of sitting beside it. Old files keep their stored value until the
field is edited.

Two things it already does that the scripts it replaces could not: it reads XMP
and atoms together, so a `rename-footage` clip shows its people, location and
rating; and `--print-json` reports `ilst_lossy` — the fields on these files that
have no iTunes atom at all, i.e. exactly what an iTunes-compatible write would
drop. (That write mode is not built. The measurement is the prerequisite for
it — DESIGN §2.1.)

Milestone 0 (the container experiment) is done and reshaped the design; its
findings are in `docs/CONTAINER.md` and reproducible with
`tests/container-experiment.sh`.

## The three things to know

**1. This library is `mdta`, not iTunes.** `~/.config/yt-dlp/config` sets
`-movflags use_metadata_tags` globally, so tags live in `moov/udta/meta` under
the `mdta` handler with arbitrary key names. The default ffmpeg path writes
iTunes `ilst` atoms instead and **silently drops** `actors`, `type` (now `variant`), `channel`,
`rating`, `origin`, `source_url`, `webpage_url`, `purl` and `yt_dlp_id` — 9 of
20 keys. The two boxes are mutually exclusive.

**2. `rename-footage` puts everything in XMP, and ffprobe cannot see it.**
People, tags, channel, location, rating and `PreservedFileName` are XMP written
by exiftool. A reader using ffprobe alone concludes a footage file has no
metadata at all. So `tagform` always runs both readers.

**3. An ffmpeg remux destroys XMP.** Totally, silently, with no flag to prevent
it. That is why the writer chooses its backend from the file's *contents* and
never from a preference: exiftool in place where it can, a remux where a key
has to be added, and a remux followed by re-applying the XMP snapshot taken at
read time where both are true. There is deliberately **no flag to override the
choice** — every such flag is a flag that lets you destroy XMP.

## Dependencies

`ffmpeg`/`ffprobe` and `exiftool`, both required at runtime.
`assets/tagform.exiftool.cfg` is a required runtime asset too: without it
exiftool refuses to write this library's custom `Keys:` tags (`Sorry, Keys:Actors doesn't exist or isn't writable`) —
the same wall `rename-footage` hit before it retreated to XMP.
