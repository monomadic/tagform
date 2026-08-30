# tagform — a form-based metadata tagger for MP4/MOV

**Status:** milestones 0–5 built, 6 largely built (see §16); 7–8 outstanding.
**Language:** Rust. **Repository:** standalone —
[monomadic/tagform](https://github.com/monomadic/tagform). **Install:**
`cargo build --release`, binary to `~/.local/bin/tagform`.

> **How to read this document.** It is a design spec written ahead of the code,
> and the code has since answered some of it. Anything marked
> **`⟨designed⟩`** is not built and describes intent, not behaviour; anything
> marked **`⟨built, differs⟩`** is built but not the way this section first
> described it, and the note says how. Everything else describes the code as it
> stands. For what actually runs today, [README.md](README.md) is authoritative
> and much shorter; for the measured container facts, so is
> [docs/CONTAINER.md](docs/CONTAINER.md).
>
> This spec predates the split from the `~/config` dotfiles monorepo, where
> `tagform` lived at `utils/tagform`. Sibling tools it refers to —
> `mp4-tui-tagger`, `ytform`, `media-audit`, `rename-footage`, `mp4doctor`,
> `media-embed`, `fd-media` — still live there and are named without links,
> since they are no longer reachable by relative path. They remain the
> behavioural references this design is measured against.

---

## 1. What this is

`tagform` is a full-screen terminal application that edits container metadata on
`.mp4` / `.m4v` / `.mov` files through **real form controls** — labelled fields
with typed editors, validation, enums, star rows, tag chips and checkboxes —
instead of a list of key/value strings.

```
tagform ~/Movies/**/*.mp4              # multi-file, aggregated like an mp3 tagger
tagform --print-json clip.mp4          # the model, as JSON
```

⟨designed⟩ Three more entry points are planned and not built: `--fetch`
(seed from yt-dlp via the embedded URL), `--from-filename` (seed from the
filename grammar), and `--set K=V --apply` (headless). See §10.

It replaces `mp4-tui-tagger`, which has the right *staging model* (nothing hits disk until `w`, multi-file
aggregation with `<multiple values>`) but the wrong *interface*: an fzf list
where every edit shells out to `$EDITOR` and every value is an untyped string.
A rating is not a string. A URL is not a string. A tag list is not a string.

Three existing things define its shape:

| Existing | What `tagform` takes from it |
|---|---|
| `mp4-tui-tagger` | staging model, multi-file value aggregation, write-on-`w` |
| `ytform` | the live form over title/actors/channel/origin/tags/rating, and the kitty-placeholder thumbnail |
| `media-audit` | thumbnail cache + cover-fit crop, faststart handling, the yt-dlp metadata fetch |
| `rename-footage` | the XMP field set, its exiftool argument order, and the field-precedence rule (§3.6) |

The difference from `ytform` is the direction of travel: `ytform` edits metadata
for a file being *downloaded*, and the output is a filename. `tagform` edits
metadata on files that already *exist*, and the output is atoms inside the
container (with the filename as an optional secondary sink).

### Non-goals

- Not a transcoder. The only ffmpeg invocation is `-c copy`.
- Not a library manager. It does not walk, index or organise; feed it paths
  (`fd-media`, `media-paths`, `fzf-media-select` already do that).
- Not a container repair tool. Fragmentation and moov placement stay
  `mp4doctor`'s job; `tagform` only rides the faststart flag along on the remux
  it is already doing.
- No Matroska, no audio-only formats. If it grows, `.mkv` goes through a
  separate backend, not by pretending MKV tags are MP4 atoms.
- Not a chapter or subtitle editor. Deferred indefinitely.

---

## 2. The container problem — read this before designing anything else

MP4 and MOV store "the same" metadata in **four** mutually incompatible places,
and which one you write decides which applications can read the file back. All
of the behaviour below was measured, not assumed — see
[docs/CONTAINER.md](docs/CONTAINER.md) for the fixtures and the numbers.

```
moov/udta/meta/hdlr = 'mdir' + ilst   →  iTunes-style four-char atoms (©nam, desc, keyw…)
                                          read by: Music/iTunes, Plex, Infuse, Jellyfin,
                                          AtomicParsley, mp4ameta, Emby
moov/udta/meta/hdlr = 'mdta' + keys/ilst → arbitrary-length string keys
                                          read by: QuickTime, Finder, AVFoundation, ffprobe,
                                          exiftool; keys can be any name at all
```

ffmpeg picks between them:

- **default** — `mdir`/ilst for `.mp4`/`.m4v`, plain `udta` for `.mov`. Only the
  keys ffmpeg has a mapping for survive; unknown keys are silently dropped.
- **`-movflags use_metadata_tags`** — `mdta`, every key preserved verbatim,
  **and the iTunes atoms are not written**.

This library commits to `mdta` everywhere. `~/.config/yt-dlp/config` says so
explicitly:

```
# let MP4 store non-standard tag names (actors, yt_dlp_*, source_url, ...)
--ppa "Metadata:-movflags use_metadata_tags"
```

and `media-embed`, `media-refresh-tags`, `media-audit` and `mp4doctor` all pass
`use_metadata_tags` on every rewrite. That is why `actors`, `source_url`,
`yt_dlp_id` and friends round-trip at all — and also why a file tagged by this
ecosystem shows up in Plex with no title.

### 2.1 Compatibility modes

**⟨designed⟩ — `--compat` is not built.** Every write today is `mdta`: the
remux always passes `use_metadata_tags`, and there is no flag to ask for
anything else. What *is* built is the diagnostic half — `--print-json` reports
`ilst_lossy`, the fields on the given files that have no iTunes atom at all,
which is exactly the set `--compat ilst` would drop. That measurement is the
prerequisite for the modes below; the modes themselves are milestone 8.

The intended surface makes this an explicit setting rather than an accident,
with three modes:

| `--compat` | What is written | Use |
|---|---|---|
| `mdta` *(default, and the only behaviour today)* | one ffmpeg pass with `use_metadata_tags`; every field, custom keys included | the house style — matches the rest of the media toolchain |
| `ilst` | one ffmpeg pass without the flag; only fields with a real atom mapping (§4) | files headed for Plex/Infuse/Music |
| `both` | the `mdta` pass, then a second in-place injection of the ilst atoms (§9.3) | archival masters; the only mode readable by everything |

**Measured (milestone 0, done).** Of 20 keys written four ways:

| Container | Flags | Box | Kept |
|---|---|---|---|
| `.mp4` | *(default)* | `[ItemList]` ilst | 11/20 |
| `.mp4` | `use_metadata_tags` | `[Keys]` mdta | **20/20** |
| `.mov` | *(default)* | `[UserData]` udta | 9/20 |
| `.mov` | `use_metadata_tags` | `[Keys]` mdta | **20/20** |

ffmpeg writes one box **or** the other, never both — so `both` genuinely needs a
second tool (§9.3). The default `.mp4` path silently drops exactly this repo's
custom vocabulary: `actors`, `type`, `channel`, `rating`, `origin`,
`source_url`, `webpage_url`, `purl`, `yt_dlp_id`.

The `.mov` default path is not merely lossy, it is *wrong*: ffmpeg invents
unnamed atoms from the first three characters of keys it cannot map, so
`description` becomes `UserData_des` and `keywords` becomes `UserData_key`, and
nothing reads those back. ⟨designed⟩ **`--compat ilst` on a `.mov` input must be
a hard error** naming the fields that would be lost — a constraint on the mode
when it lands, not something that can be wrong today, since the mode does not
exist.

One more measured consequence: with `-map_metadata 0`, `use_metadata_tags`
copies `major_brand`, `minor_version` and `compatible_brands` in as *real*
readable tags, which then accumulate on every rewrite. Hiding them from the form
(§3.6) is not enough — every write must actively clear them with
`-metadata major_brand=` and friends.

### 2.2 XMP — the fourth namespace, and the dangerous one

`rename-footage` stores **all** of its authored metadata as XMP via exiftool,
not as atoms at all. Two measured facts govern the entire write path:

- **ffprobe cannot see XMP.** A file carrying six XMP fields reports exactly the
  same `format_tags` as one carrying none.
- **An ffmpeg remux destroys XMP**, totally and silently, with no flag to
  prevent it. The `[Keys]` tags survive; the XMP does not.

So a `tagform` that always remuxed would erase everything `rename-footage`
authored — including `PreservedFileName`, which that script's comments call "the
only surviving copy" of a file's original name. The writer therefore chooses its
backend from the file's *contents*, never from a user preference (§9.2).

---

## 3. The field schema

A **field** is what the user sees: one label, one control, one value. A **key**
is what lands in the container. The relation is one-to-many — the URL field
writes five keys — and that fan-out is the whole reason this tool exists.

### 3.1 The primary fields

The ten the brief requires, plus the ones the yt-dlp config already produces and
would otherwise be silently dropped on every rewrite. This table is now
generated-from-life: it mirrors `FIELDS` in `src/model/schema.rs`, which is the
authority.

| # | Field | Control (§5) | Container keys (`mdta`) | XMP | ilst atom |
|---|---|---|---|---|---|
| 1 | **Title** | Text | `title` | `XMP-dc:Title` | `©nam` |
| 2 | **Actors** | List (chips, `,`) | `actors`, `artist` | `XMP-iptcExt:PersonInImage` | `©ART` |
| 3 | **Artist** | Text | `artist` | — | `©ART` |
| 4 | **Rating** | Stars 0–5 | `rating` | `XMP-xmp:Rating` | — |
| 5 | **Description** | TextArea | `description` | `XMP-dc:Description` | `desc` |
| 6 | **URL** | URL (validated) | `webpage_url`, `source_url`, `purl`, `comment`, `original_url` | — | `purl` |
| 7 | **Channel** | Text | `channel`, `album_artist`, `album` | `XMP-xmpDM:Album` | `aART` |
| 8 | **Tags** | HashTag chips | `keywords` | `XMP-dc:Subject` | `keyw` |
| 9 | **Genre** | Enum | `genre` | — | `©gen` |
| 10 | **Type** | Enum | `type` | — | — |
| 11 | **Kind** | Enum (closed) | `media_type` | — | `stik` |

Read aliases are wider than the write set (§4.2): Actors also reads `cast`,
Tags also reads `keyw`.

⟨built, differs⟩ Four claims in the original table did not survive contact:

- **Rating writes one key, not two.** There is no `comment` JSON blob and no
  freeform `com.apple.iTunes:rating` atom. `rating` in `mdta`, `XMP-xmp:Rating`
  where XMP is present, and nothing in `ilst` — the field is one of the five
  `--print-json` reports as `ilst_lossy`.
- **Actors does not write `iTunMOVI`.** The plist blob is still deferred
  (§17.5), so Apple software does not see the cast list.
- **Channel writes `aART` only**, not `©alb` and `tvnn` as well.
- **Description does not overflow into `ldes`.** Synopsis owns `ldes`; the
  >255-byte split (§5.2) is ⟨designed⟩.

⟨designed⟩ The `⌃T` title-case helper (1) and Channel completion (7) are not
built; see §5.1.

### 3.2 Secondary fields

⟨built, differs⟩ **There is no collapsed *More ▸* section.** The form is one
flat list. Three of the fields below were promoted into it outright, and the
rest are unbuilt.

Built, sitting directly after Kind:

| Field | Control | Keys (`mdta`) | Read also | XMP | ilst |
|---|---|---|---|---|---|
| Date | Date (`YYYY-MM-DD`) | `date` | `com.apple.quicktime.creationdate` | `XMP-xmp:CreateDate` | `©day` |
| Synopsis | TextArea | `synopsis` | — | — | `ldes` |
| Origin | Text | `origin` | — | — | — |

Date deliberately does **not** read `creation_time`: that is muxer bookkeeping
(§3.7), and treating it as authored would show every file carrying a date nobody
set. `com.apple.quicktime.creationdate` is different — a phone writes it, and it
is a real capture time. `date` is read first, so an edit written there wins on
the next read.

⟨designed⟩ Not built, and unlikely to be until something wants them: Comment,
Composer, Director, Producer, Studio, Copyright, Grouping, Language, Show,
Season / Episode, Episode ID, Advisory, Content rating. Their key mappings are
recorded in git history; three of them (Director, Producer, Studio) are blocked
on `iTunMOVI` (§17.5) for any Apple-visible result, and the rest are TV-library
metadata this collection does not use. **The reason to add one is a file that
carries it**, not completeness — every field added to the flat form costs a row
on every screen.

### 3.3 Three different things called "rating"

This trips up every MP4 tagger and the schema must keep them apart:

1. **Stars, 0–5.** The user's own convention. Lives in the filename as a
   trailing ` ★★★☆☆` (`media-set-rating`, `media-parse-filename-to-json`) and in
   the `comment` JSON blob `media-write-tags` emits. **There is no standard atom
   for it** — iTunes keeps star ratings in its library database, not in the
   file. `tagform` writes it to the `rating` key, and to `XMP-xmp:Rating` on
   files that carry XMP, which is a real standard 0–5 field. ⟨designed⟩ The
   freeform `com.apple.iTunes:rating` atom is not written; it was a guess, and
   §17.2 is where it stays until someone checks what Plex and Infuse read.
   ⟨designed⟩ Filename sync (§9.4) is not built.
2. **Advisory** (`rtng`): `0` none / `2` clean / `1` explicit. ⟨designed⟩.
3. **Content rating** (`iTunEXTC`): `mpaa|R|400|`, `us-tv|TV-MA|600|`.
   ⟨designed⟩.

Field 4 is sense (1). The other two are never conflated with it — and are the
easier discipline to keep now that neither is built.

### 3.4 Two different things called "type"

Same discipline:

- **Type** (field 10) is the user's own axis, and it already exists — the yt-dlp
  config has `--alias clip/master/original` writing `meta_type`. ⟨built,
  differs⟩ Intended as an *open* enum; built closed, like Kind (§5.7).
- **Kind** (field 11) is `stik`, a closed integer enum the Apple ecosystem
  actually reads:

  | `stik` | Label |
  |---|---|
  | 0 | Home Video |
  | 1 | Normal |
  | 2 | Audiobook |
  | 6 | Music Video |
  | 9 | Movie |
  | 10 | TV Show |
  | 21 | Podcast |

  Rendered as labels, stored as the integer. Default `9` (Movie) for video
  files with no existing value, or `10` when Show is non-empty.

### 3.5 Enum sources — the yt-dlp config is the schema

The genre and type enums are not invented here. They are exactly the aliases in
`config/yt-dlp/config`:

```
--alias media    '… --parse-metadata "Media:%(meta_genre)s"'
--alias footage  '… --parse-metadata "Camera Footage:%(meta_genre)s"'
--alias karaoke  '… --parse-metadata "Karaoke:%(meta_genre)s"'
--alias vj       '--parse-metadata "VJ Clip:%(meta_genre)s"'

--alias clip     '… --parse-metadata "Clip:%(meta_type)s"'
--alias master   '… --parse-metadata "Master:%(meta_type)s"'
--alias original '… --parse-metadata "Original:%(meta_type)s"'
```

→ Genre: `Media`, `Footage`, `Karaoke`, `VJ Clip`
→ Type: `Clip`, `Master`, `Original`

**`Footage`, not `Camera Footage`.** The yt-dlp alias literal is currently
`Camera Footage`; `tagform` normalises it. A `[enums.aliases]` table maps stored
values to canonical ones on read, so existing files tagged `Camera Footage`
display and re-save as `Footage` without a migration pass.

⟨built, differs⟩ The normalisation is real but it is **hard-coded in
`config.rs`, not configurable** — there is no `config.toml` and so no
`[enums.aliases]` table (§12). `Camera Footage` → `Footage` is the only alias
that exists, which is the only one anything has needed.

Changing the yt-dlp alias itself (`config/yt-dlp/config`, the `--alias footage`
line) is a separate one-line edit that only affects *new* downloads; the alias
table above is what makes the two agree either way.

Hard-coding them would guarantee drift the first time an alias is added, so
`tagform` **parses `~/.config/yt-dlp/config` at startup**: any
`--alias NAME '...meta_genre...'` or `...meta_type...` line contributes its
literal to the enum. Config `enums.genre` / `enums.type` (§10) extend or
override. Parse failure is not fatal — it falls back to the four/three above and
notes it in the status line.

### 3.6 The Footage profile: XMP fields from `rename-footage`

**Built**, and it is the part of this document the code changed most.

Actors, Channel, Tags and Rating are not separate footage fields at all — they
are the §3.1 fields, which carry an XMP tag alongside their atoms and resolve
XMP-first (§4.1). That was the intent all along ("the same fields, only their
storage differs"), and it is what shipped.

What is genuinely footage-specific is a group of five fields that appear only
when the file actually carries them:

| Field | Control | XMP tag | Note |
|---|---|---|---|
| **Location** | Text | `XMP-iptcExt:LocationCreatedCity` | a place name, and only that |
| **State** | Text | `XMP-iptcExt:LocationCreatedProvinceState` | |
| **Country** | Text | `XMP-iptcExt:LocationCreatedCountryName` | |
| **Coordinates** | ReadOnly | — (atoms `location`, `location-eng`) | the ISO 6709 string the camera wrote |
| **Original name** | ReadOnly | `XMP-xmpMM:PreservedFileName` | write-once |

⟨built, differs⟩ Three corrections to the original design:

- **The gate is the value, not the Genre.** A footage field is shown when it is
  present in at least one file in the selection and hidden when it is `Absent`
  — `footage_only` in `schema.rs`, checked in both `build_rows` and
  `--print-json`. Keying it on `Genre == Footage` would have hidden the
  location on every clip whose genre was never set, which is most of them.
- **State and Country were added.** `rename-footage --geocode` writes the city
  as one field of an IPTC block and fills the province and country in beside
  it, deliberately, so that the plain-text place and the numbers it came from
  live in one structure. Editing the city without those two visible is how they
  drift apart.
- **Coordinates is its own read-only field.** It deliberately does not feed
  Location: ffmpeg maps QuickTime's `com.apple.quicktime.location.ISO6709` onto
  the `location` key, so a shared field displayed
  `+13.7165+100.5867+018.071/` as though it were a city — and an edit would
  have written a place name over a coordinate.

Notes that are not optional:

- **Actors/Channel/Tags/Rating are the *same fields* as §3.1**, not new ones.
  Only their storage differs. The Footage profile changes where a field is
  written, never what the user sees. `rename-footage` already reads the atoms as
  a fallback, and `tagform` does the same in both directions.
- `XMP-xmp:Rating` is a **standard 0–5 rating field** — which largely settles
  open question 2. On Footage files the stars have a real home; the freeform
  atom is only needed elsewhere.
- **`PreservedFileName` is write-once and read-only in the form.** It is the
  only record of a camera's original `IMG_4855.MOV`. `tagform` displays it,
  offers `⌃T` to copy it into Title, and never overwrites it. If it is absent
  and the file is being renamed, `tagform` stamps it — same rule as
  `rename-footage`.
- **XMP list tags do not replace on assignment, they append.** Clearing requires
  an empty assignment *first*, and the values that follow must use `=` and not
  `+=`, because an append is applied against the original list and survives the
  clear — quietly doubling the list on every run. `rename-footage`'s
  `build_metadata_args()` documents this trap; `tagform` reuses its exact
  argument order.
- **The filename is a source, not just a sink.** `rename-footage` resolves every
  field by a fixed precedence, and `tagform` follows it so the two cannot
  disagree about the same file:

  1. an explicit edit wins outright and is written; for a list, the edited
     values *are* the list, replacing what was stored. The filename is not
     consulted.
  2. no edit, field already has metadata → keep it; ignore the filename.
  3. no edit, no metadata → parse it out of the filename **and embed it**.

  Rule 3 is what keeps the name disposable without it ever being the only copy
  of something. It is also why `--from-filename` (§10) is not really an optional
  seeding mode: for an empty field it is the *default* behaviour, and only the
  writing of it back is opt-in.
- **A camera's own name is not a title.** `IMG_4855`, `GX010042`, `C0001` and
  friends are recognised and refused for the Title field under rule 3;
  `PreservedFileName` already holds them verbatim.
- Kind (`stik`) defaults to `0` (Home Video) when Genre is `Footage`.
- Device (`com.apple.quicktime.model`) and the `[RES FPS LENGTH …]` spec block
  are **probed, never authored** — shown in the header line, never editable.

### 3.7 Keys `tagform` never shows

`major_brand`, `minor_version`, `compatible_brands`, `encoder`, `handler_name`,
`vendor_id`, `creation_time` — muxer bookkeeping, hidden from the form, and
actively cleared on every write rather than merely ignored (§2.1).

Everything else found on disk but absent from the schema appears as **Custom**
rows at the bottom of the form, so no existing tag is ever lost by being
unrecognised. `yt_dlp_extractor`, `yt_dlp_id`, `yt_dlp_slug`,
`yt_dlp_info_json` and their siblings land here.

⟨built, differs⟩ Two changes:

- **Custom covers XMP as well as atoms.** Rows are keyed by origin —
  `custom:<atom key>` or `xmp:<tag>` — because the write plan has to put an
  edit back where it came from. This was added after `rename-footage` grew its
  IPTC location block: an XMP tag no field claimed was *invisible*, preserved
  on write but with nothing on screen saying it was there.
- **They are plain editable text rows, not read-only.** There is no
  `--edit-custom` flag. Making provenance read-only by default is still
  defensible, but nothing has yet been damaged by its absence, and a flag that
  gates an edit nobody makes is a flag that only ever gets in the way.

---

## 4. Reading and writing keys

### 4.1 Read

**Two readers, always.** `ffprobe -v error -show_entries format_tags -of json`
for the atoms, and `exiftool -j -G1 -n -XMP:all` for the XMP that ffprobe is
blind to (§2.2). Keys are lower-cased for lookup; the original casing is
retained for round-tripping unrecognised keys. exiftool exits non-zero on a
file with no XMP at all — that is the common case for a plain download, not an
error, and the reader treats it as such.

Reading with ffprobe alone would report every footage file as having no people,
no tags, no channel, no location and no rating — and the form would then offer
to write that emptiness back. exiftool is a hard dependency of the *read* path,
not an optional enhancement.

Precedence per field is XMP → atoms, matching `rename-footage`'s
`first_present()`. A value seen in neither is `Unset`; a value seen in both that
disagrees is surfaced in the inspector rather than silently resolved.

A second `ffprobe` call on the stream list supplies the header line:
resolution, duration, codecs, bitrate, and stream-level `tags` (which the form
does not edit, but must not clobber — hence `-map_metadata 0` on write). The
write path runs its own `-show_streams` to capture the shape it has to
reproduce (§9.2.1).

⟨built, differs⟩ **Tag probing is sequential and blocking**, not a worker pool:
`main.rs` probes every file up front and the UI opens once they are all in.
What *is* off-thread is the per-file media probe and the thumbnail extraction,
each on its own thread reporting through an `mpsc::Sender<Msg>`, because those
seek through multi-gigabyte files. The skeleton-then-fill design was written
for a 40-file SMB selection; nothing has yet been slow enough to need it, and
it is the obvious first move if that changes.

### 4.2 The mapping table is data, not code

```rust
struct KeyMap {
    field:   FieldId,
    mdta:    &'static [&'static str],  // every key this field writes in mdta mode
    ilst:    Option<Ilst>,             // Fourcc | Freeform{mean, name} | ITunMovi(role)
    read:    &'static [&'static str],  // aliases accepted on read, first match wins
}
```

`read` is wider than `mdta` on purpose: a file might carry `purl` but not
`webpage_url`, or `cast` but not `actors`. Read accepts any alias; write emits
the canonical set. That asymmetry is what makes the tool idempotent across files
tagged by different generations of these scripts.

The ilst column in §3 is **measured** rather than asserted: `rating`, `type`
and `origin` have no ilst mapping at all and exist only because
`use_metadata_tags` allows arbitrary keys (§2.1). `--print-json` reports that
set per selection as `ilst_lossy`, computed from `FIELDS` rather than from a
sidecar.

⟨built, differs⟩ **There is no checked-in `keymap.json`.** The table lives in
`FIELDS` and nowhere else, which is the right call for a table with one
consumer — a generated file checked in beside the code it duplicates is a
second source of truth waiting to drift.

One measured trap for the read path: a value can be written by exiftool and read
back by exiftool while ffprobe reports it as **empty** — observed on a large
`.mov` whose padding atom had been consumed, and *not* predicted by value size
(docs/CONTAINER.md §4). `tagform` therefore never writes an empty value over a
field whose two readers disagree about emptiness, and warns on the
disagreement — which is observable — rather than on a byte count, which is not.

### 4.3 Multi-file aggregation

Straight from `mp4-tui-tagger`, which got this right:

⟨built, differs⟩ The four-state enum was split in two, which is the better
shape: `Agg` in `model/value.rs` describes **what is on disk**, and staged
edits live separately in the app rather than as extra variants of it. A state
that means "the user did something" has no business in a type produced by the
reader.

What the reader produces:

| `Agg` | Meaning | Display |
|---|---|---|
| `Absent` | present in no file | `—` |
| `Same { value }` | present and identical in every file | the value |
| `Mixed { values }` | differs, or present in only some | `‹multiple›`, dimmed |

`Agg::value()` returns `Some` only for `Same`: neither `Mixed` nor `Absent` has
a single value an edit could be compared against, and collapsing them to one
would be how a batch silently flattens.

On top of that, a staged edit marks its row `●` and is what `w` writes. `Mixed`
is preserved: a field left alone keeps each file's own value, and only edited
fields touch disk. A `Mixed` field shows its per-file values in the inspector
(`p`), and setting it says how many distinct values it is about to flatten —
in the confirmation, before it happens.

For list-valued fields `Mixed` additionally offers **merge** — `m`, not `M` —
the union of every file's values in first-seen order, folded
case-insensitively. That is the operation you actually want when tagging a
batch, and none of the scripts this replaces can do it.

---

## 5. Controls

The heart of the app.

⟨built, differs⟩ **One `Editor` enum, not a `Control` trait.** The design called
for a trait object per control and a `Vec<Box<dyn Control>>`; what shipped is a
single enum in `ui/edit.rs` with a variant per control, because only one field
is ever being edited at a time. The form holds `Vec<Row>` — key, label,
`Control` discriminant, `Agg` — and materialises an `Editor` for the focused
row on `enter`. Ten variants and no vtable, and the whole edit surface is one
`match` you can read top to bottom.

`Reaction` survived and is what makes navigation work: `Consumed | Pass |
Commit | Cancel`. A control that does not consume a key hands it back to the
form, so a `TextArea` keeps `↑`/`↓` for the cursor while a one-line field lets
them move rows. `Control::ReadOnly` returns `Pass` for everything, which is the
entire implementation of a non-editable field.

Built: Text, TextArea, List, HashTags, Url, Stars, Enum, Date, ReadOnly.
⟨designed⟩ Checkbox (§5.8) and Number (§5.9) are not built — nothing needs
them until the write panel and the TV fields land.

### 5.1 Text

`tui-input` under the hood: cursor, horizontal scroll for values wider than the
field, home/end/word-motion, and a masked variant that is unused here but free.
Rendered as a single line inside a `▏ ▕` gutter that colours by validation state.

**The emacs/macOS control keys are bound**, because a text field on this
machine is expected to answer to them and they are not this program's to
invent:

| key | |
|---|---|
| `⌃A` / `⌃E` | start / end of line |
| `⌃B` / `⌃F` | back / forward one character |
| `⌃D` / `⌃H` | delete the character right / left |
| `⌃W` | delete the word behind the cursor |
| `⌃K` | delete to end of line |
| `⌃U` | clear the line |

⌃U is the macOS reading — the whole line, not readline's discard-to-the-left.
⌃W and ⌃K already cover the partial kills between them, so the one that clears
outright is the useful third. ⌃H arrives as either the letter or `Backspace`
carrying the modifier, depending on the terminal; both spellings are bound.

They live on `line_key`, so every text-backed control gets them — Text,
TextArea, URL, Date and the list/hashtag chips, which are one joined line
underneath (§5.3). Stars and Enum have no text and pass them straight through.

This is what the mode split (§11) bought. Taking eight control keys costs
nothing here because they bind **only while a field is open**; Select mode's
single-letter commands are untouched.

⟨designed⟩ **Completion is not built**, and it is the largest single piece of
ergonomics still missing — 90% of tagging is retyping a channel name you have
typed before. The plan stands: `⌃Space` opens a dropdown of values seen in the
current selection plus a frecency list, filtered with `nucleo`. It is milestone
7, and it wants the value history in §12 to exist first.

### 5.2 TextArea

`ratatui-textarea` for Description / Synopsis / Comment. Soft-wrapped, grows to
`min(content, 8)` rows and scrolls beyond that. `⌃E` opens `$EDITOR` on a temp
file for anything longer, then reads it back — keeping `mp4-tui-tagger`'s escape
hatch without making it the only path.

Description validates: over 255 bytes emits `Warn("N bytes; over 255 some
readers truncate")`. Warnings never block a write.

⟨designed⟩ The `$EDITOR` escape hatch and `⌃L` (move the overflow into
Synopsis) are not built — the warning names the problem without yet offering
the fix. **The escape hatch needs a new key**: `⌃E` is end-of-line (§5.1), and
a text field that answers to the emacs keys everywhere except one letter is
worse than one that has no escape hatch at all.

### 5.3 List (Actors, Director, Producer)

Chips on one line: `Sasha Grey · Manuel Ferrara · +`.

Stored comma-joined (`Sasha Grey, Manuel Ferrara`), matching what yt-dlp's
`%(cast)l` produces, and split on commas on the way in (`ytform`'s `SplitList`
rule).

⟨built, differs⟩ **Editing is line-wise, not chip-wise.** A list field edits as
its comma-joined text and re-splits on commit. Per-chip focus, `⌥←`/`⌥→`
reordering, `⌫`-reopens-previous and the `… +3` overflow collapse are all
⟨designed⟩. The chips are a *rendering*, which is most of the value for a
fraction of the state machine — and `m` (merge, §4.3) turned out to matter far
more for batch work than chip navigation would have.

### 5.4 HashTag (Tags)

A List with a different grammar, because tags round-trip through *filenames*:

- displayed `#anal #pov #hd`, always with the `#`
- input accepts `#anal`, `anal`, `anal, pov`, `anal pov` — split on comma **or**
  space, leading `#` stripped then re-added (`ytform`'s `SplitTags`)
- the sanitiser mirrors the yt-dlp config's
  `--replace-in-metadata "tags" "[ _]+" "-"`: internal whitespace and
  underscores become `-`, so a tag is always one filename token
- **stored comma-joined without `#`** in `keywords` — `#` is presentation
- `Warn` on a tag containing `/`, `\`, `:` or a leading `.` (filename-hostile)
- ⟨designed⟩ `⌃Space` completes against the corpus of tags seen across the
  library index

### 5.5 URL

Text plus a `url::Url` parse on every keystroke:

| Condition | State |
|---|---|
| empty | `Ok` (absent is legal) |
| parses, scheme `http`/`https` | `Ok`, host shown dimmed to the right |
| parses, other scheme | `Warn("unusual scheme")` |
| no scheme but looks like a host | `Warn` + `⌃F` fixes it by prefixing `https://` |
| unparseable | `Error` — blocks write |

The table above is built exactly as written, down to the message text
(`not a URL: …`).

⟨designed⟩ `⌃O` (open), `⌃Y` (yank), the `https://` prefix fix-up and — the one
that matters — **fetch** are not built. Fetch and the fix-up both need keys
that are not `⌃F`, which is forward-one-character (§5.1). Fetch runs
`yt-dlp --skip-download
--dump-single-json` against the URL, via the shared cache `media-audit` and
`ytq` already use, and offers to fill Title, Actors, Channel, Description, Tags
and Date from the result with a per-field diff, so nothing is silently
overwritten. It is `media-refresh-tags` as an interaction instead of a script,
and it is the headline feature of milestone 7.

Recognising a URL is already embedded is why the URL field reads five aliases
(§4.2): files in this library carry it as `comment` (old `media-write-tags`
output), `purl` (yt-dlp), `source_url` and `webpage_url` (`media-embed`), or
`original_url` (`media-audit`). All five are read; all five are written.

### 5.6 Stars (Rating)

The control from `media-set-rating`, made reusable:

```
  Rating      ★★★☆☆
```

`←`/`→` or `h`/`l` step, and `h`/`l` also step it from Select mode without
entering an edit at all. Renders five glyphs always (filled + hollow), which is
the exact form the filename grammar parses back.

⟨designed⟩ `0`–`5` to set directly and `j`/`k` for clear/full are not bound:
in the modal design (§11) those keys move between rows, and taking them back
for one control would be exactly the inconsistency the mode split bought.

### 5.7 Enum

**Built as one fixed set, drawn inline.** Opening a field expands its options
along the same row it already occupied, with the current one lit — same row,
same height, so nothing below it reflows:

```
   Type             Clip                       ← closed
  ▶Type              Clip  Master  Original    ← open, "Clip" lit
```

`←`/`→` or `h`/`l` step and wrap, `⏎` accepts, `⇥` accepts and advances,
`j`/`k` accept and move a row, `esc` reverts the field. Stepping only moves a
*pending* value; nothing is staged until it is accepted, which is what lets
`esc` back out cleanly. In Select mode `h`/`l` step the set without opening it.
Opening a field that is empty lands on the first option, so there is always
something lit to step away from — and an unset field still reads as `—` while
you are merely moving past it.

**No free text, for now.** The open/closed split the plan called for is not
built: typing into a set is rejected outright, and Genre and Type are picked
from the list like Kind is. What keeps that from losing data is that a value
already on the file which the list does not know **joins the set for that
field** — an unfamiliar Genre is lit, selectable and steppable, it just cannot
be typed. Free-text entry comes back later.

### 5.8 Checkbox

⟨designed⟩ **Not built, and mostly not needed.** The three options it was for
resolved differently:

- **MOV faststart** is real, defaults on, and toggles with `f` from Select mode
  — a single key, no control and no panel.
- **Sync filename** depends on §9.4, which is unbuilt.
- **Back up originals** was dropped. The write path never modifies an original
  until a verified replacement exists (§9.2.1), which is the guarantee a backup
  was standing in for; keeping a second copy of a 6 GB file to insure against a
  failure mode that leaves the original untouched is cost without cover.

A Checkbox control arrives when a second toggle does.

### 5.9 Date, Number

Date: accepts `YYYY-MM-DD`, warns and normalises the `YYYYMMDD` form yt-dlp's
`upload_date` uses, and warns on anything else. ⟨designed⟩ Auto-inserted
dashes, `↑`/`↓` on the segment under the cursor, and `t` = today are not built.

⟨designed⟩ Number is not built; nothing uses it until the Season/Episode fields
land (§3.2).

### 5.10 Validation model

`validate()` runs per keystroke; the form aggregates:

- any `Error` → the write key is inert and the status bar names the first
  offending field. Exactly one thing produces `Error` today: an unparseable
  URL. (The non-integer Number case waits on the control.)
- `Warn` → yellow gutter, listed in the confirmation dialog, never blocks.

Errors are rare by design. A tagger that refuses to save because it dislikes
your description is a worse tool than one that saves it.

---

## 6. TUI library choice

### 6.1 Survey

*Kept as the record of why the dependency set is what it is. The verdicts all
held; nothing here has been revisited since, and the versions below are the
ones in `Cargo.toml` today.*

| Crate | Latest | Health | Verdict |
|---|---|---|---|
| [`ratatui`](https://crates.io/crates/ratatui) | 0.30 | the ecosystem standard; split into `ratatui-core`/`ratatui-widgets` at 0.30 | **base** — already what `leaf` uses |
| [`crossterm`](https://crates.io/crates/crossterm) | 0.29 | standard backend | **base**, with `use-dev-tty` on macOS as `leaf` does |
| [`tui-input`](https://crates.io/crates/tui-input) | 0.15.4 (Aug 2026) | ~494k recent downloads, actively maintained | **yes** — single-line editing, backend-agnostic, tiny |
| [`ratatui-textarea`](https://crates.io/crates/ratatui-textarea) | 0.9.2 (Jun 2026) | the ratatui-org fork of `tui-textarea`, tracks 0.30 | **yes** — multi-line fields |
| [`tui-textarea`](https://crates.io/crates/tui-textarea) | 0.7.0 (Oct 2024) | 725k downloads but stalled on ratatui 0.29 | no — the fork above supersedes it |
| [`ratatui-image`](https://crates.io/crates/ratatui-image) | 11.0.6 (Jun 2026) | ~301k recent downloads; kitty (incl. unicode placeholders), sixel, iterm2, halfblocks | **yes** — §8 |
| [`rat-widget`](https://crates.io/crates/rat-widget) | 3.2.1 | complete (checkbox, choice, focus, forms) but ~2.9k recent downloads and its own event/focus framework | no — adopting its framework costs more than writing eight controls |
| [`ratatui-interact`](https://crates.io/crates/ratatui-interact) | young | checkbox/input/button/select + focus + mouse | no — right idea, too new to depend on; revisit at v2 |
| `ratatui-form`, `ratatui-select` | young / placeholder | | no |
| [`tui-realm`](https://crates.io/crates/tuirealm) | mature | Elm-ish component framework over ratatui | no — the message indirection buys nothing at this size |
| `cursive` | mature | retained-mode, its own backend | no — will not compose with `ratatui-image` |
| `iocraft` | growing | React-like, hooks | no — different paradigm, weaker image story |

### 6.2 Recommendation

**`ratatui` + `crossterm`, with a hand-written control layer**, borrowing
`tui-input` for line editing, `ratatui-textarea` for multi-line, and
`ratatui-image` for thumbnails.

The reasoning: of the eleven controls in §5, exactly two (Text, TextArea) are
generic. The other nine — star row, hashtag chips with filename-safe
sanitisation, a URL field that fetches yt-dlp metadata, an enum sourced from a
yt-dlp config file — are domain controls that no widget library will ever ship.
A form framework would be adopted for two controls and fought for nine. The
`Control` trait in §5 is roughly 150 lines; the framework's focus/event model
would be more than that just to integrate.

This also keeps the dependency profile in line with `leaf`, the repo's other
ratatui program, so there is one ratatui version to track rather than two.

What shipped, verbatim from `Cargo.toml`:

```toml
[dependencies]
ratatui          = "0.30"
crossterm        = "0.29"
ratatui-image    = "11"
tui-input        = "0.15"
ratatui-textarea = "0.9"
url              = "2"
unicode-width    = "0.2"
image            = "0.25"
anyhow           = "1.0"
serde            = { version = "1.0", features = ["derive"] }
serde_json       = "1.0"

[target.'cfg(target_os = "macos")'.dependencies]
crossterm        = { version = "0.29", features = ["use-dev-tty"] }
```

Two differences from the plan: `image` was added (`ratatui-image` takes a
decoded `DynamicImage`, so the thumbnail path decodes the JPEG itself), and
`nucleo` and `toml` are absent because the features that wanted them —
completion (§5.1) and the config file (§12) — are not built. Neither is a
dependency worth carrying ahead of its feature.

### 6.3 Event loop and focus

Single-threaded UI; the media probe and thumbnail extraction run on worker
threads and report through an `mpsc::Sender<Msg>`. The loop selects over
crossterm events and that channel.

⟨built, differs⟩ Tag probing and the write pass are **not** off-thread — the
first happens before the UI opens (§4.1), the second blocks behind a
confirmation the user is already waiting on. A write that streams its progress
back through `Msg` is worth doing when a batch gets large enough to want a
progress bar; a 40-file batch has not yet been that.

```
crossterm events ─┐
                  ├─▶ App::update(Msg) ─▶ ratatui draw ─▶ /dev/tty
worker msgs ──────┘
```

Focus is an index into the row list, moved with `j`/`k` and the arrows in
Select mode and by `tab`/`shift-tab` from either mode. There are no hidden
sections to exclude: every row, Custom included, is in the ring (§3.2, §3.7).

⟨designed⟩ Mouse support and `--mouse` are not built; terminal text selection
keeps working by default, which was the point of the flag.

⟨designed⟩ **`/dev/tty` is not opened explicitly** — the app uses crossterm's
default stdout backend, with `use-dev-tty` on macOS for input only. Composing
inside `$(...)` or under `fzf --bind execute(...)` therefore is not yet
supported. It matters for the §15 integrations and nothing else, so it lands
with them.

### 6.4 Undo

Undo is a snapshot stack of the whole staged-edit map
(`Vec<BTreeMap<String, Value>>`), pushed on every committed edit rather than
every keystroke. `u` undoes, `ctrl-r` redoes, and `r` reverts every staged edit
at once. Cheap at this data size and immune to the subtle bugs a per-control
undo would have.

⟨built, differs⟩ It snapshots the *staged edits*, not the form state, and it is
**unbounded** rather than capped at 200 — a session's edits are a few hundred
short strings, and a cap that never binds is a cap that only ever surprises
someone. It is cleared on write, since undoing across a write would suggest a
disk change could be taken back.

---

## 7. Layout

The sketch below is the original design. It is still the right shape, and three
things in it are ⟨designed⟩ rather than built: the collapsed **More** and
**Custom** rows (both sections are flat now — §3.2, §3.7), the **write** panel
of checkboxes (§5.8), and the `(auto)` Artist mirror (§17.4). What replaced the
write panel is a single `f` toggle and a shortcut strip that lists only the
keys live in the current mode.

```
┌─ tagform ──────────────────────────────── 3 files · 2 changed · mdta ─┐
│ ▛▀▀▀▀▀▀▀▀▀▀▀▀▜  Sasha Grey - [Brazzers] Some Title #pov (fh_881).mp4  │
│ ▌            ▐  1920×1080 · 24:11 · h264/aac · 1.4 GB · faststart ✓   │
│ ▌ thumbnail  ▐  ~/Movies/Porn/Downloads                               │
│ ▙▄▄▄▄▄▄▄▄▄▄▄▄▟  ‹ 1/3 ›                                               │
├───────────────────────────────────────────────────────────────────────┤
│  Title        ▏Some Title                                          ▕  │
│  Actors       ▏Sasha Grey · Manuel Ferrara · +                     ▕  │
│  Artist       ▏Sasha Grey, Manuel Ferrara                     (auto)▕  │
│  Rating        ★★★★☆                                                  │
│  Description  ▏Lorem ipsum dolor sit amet, consectetur adipiscing  ▕  │
│               ▏elit, sed do eiusmod tempor.                        ▕  │
│  URL          ▏https://faphouse.com/videos/881          ✓ faphouse ▕  │
│  Channel      ▏Brazzers                                            ▕  │
│  Tags         ▏#pov #hd #anal +                                    ▕  │
│  Genre        ▏‹ Media ›                                           ▕  │
│  Type         ▏‹ Clip ›                                            ▕  │
│  Kind         ▏‹ Movie ›                                           ▕  │
│                                                                       │
│  ▸ More (16)                        ▸ Custom (7)                      │
├─ write ───────────────────────────────────────────────────────────────┤
│  [✓] MOV faststart   [ ] Sync filename   [ ] Back up originals        │
├───────────────────────────────────────────────────────────────────────┤
│ ⇥ field  ⌃Space complete  ⌃F fetch  ]/[ file  w write  u undo  q quit │
└───────────────────────────────────────────────────────────────────────┘
```

What the form actually paints: a filled `tagform` badge heading the screen,
every field showing a coloured editable region whether or not it is focused,
`▍` on the focused row (`▶` while editing) and `●` on a staged one, and the
shortcut strip along the bottom. Colours are true-colour in four schemes —
`midnight`, `gruvbox`, `nord`, `rose-pine` — cycled with `c` or picked with
`--theme=NAME`.

A test computes WCAG contrast for every text colour in every scheme against
that scheme's own background and fails below 3:1, and checks that a custom-key
label is a different *hue* from an ordinary one rather than a dimmer shade.
Both guards exist because both mistakes were made: 16-colour `DarkGray`
labels, and a file path drawn in a divider colour at 1.4:1.

The **inspector** (`p`) replaces the thumbnail band with a per-file value table
for the focused field — the answer to "what does `‹multiple›` actually contain",
which `mp4-tui-tagger` could only show in an fzf preview.

---

## 8. Thumbnails

Cheap to add here because two working implementations already exist in-repo.

**Rendering** — `ratatui-image`, which detects the terminal's best protocol
(kitty with unicode placeholders, sixel, iTerm2, halfblocks) and provides both a
stateful widget for animated resizing and a static one. The unicode-placeholder
path is the important one: the image is transmitted once and the cells hold real
text (`U+10EEEE` plus row/column diacritics), so an immediate-mode redraw does
not tear the image. `utils/ytform/thumb.go` implements that protocol by hand and
is the reference if `ratatui-image` ever has to be dropped — and the fallback
ladder is kitty → sixel → halfblocks → nothing, never an error.

**Extraction** — `media-audit`'s recipe, with one deliberate reversal:

```
ffmpeg -v error -y -ss 2 -i FILE -frames:v 1 \
  -vf "scale=w=W:h=H:force_original_aspect_ratio=decrease:flags=lanczos" \
  -q:v 3 -- OUT.jpg
```

Seek 2 s in to clear black leader, fall back to frame 0 on failure.

⟨built, differs⟩ **`decrease`, not `increase`, and no crop.** `media-audit`
cover-fits because it fills a fixed band in a report. Here the frame is the
only thing on screen telling you which file you are editing, and cropping it
to fill the box made a vertical phone clip render as a 720×404 centre-cropped
strip — the preview claimed the video was landscape. Fitting inside the box
leaves letterboxing; asserting the aspect ratio it does not have is worse. A
test pins `force_original_aspect_ratio=decrease` in place.

**Cache** — `${XDG_CACHE_HOME:-~/.cache}/tagform/thumbs/<hash>.jpg`, keyed on
`path:mtime:size:boxW×boxH`. Including the box dimensions means a terminal
resize regenerates rather than stretching a stale crop; including mtime+size
means a re-encoded file gets a new thumbnail. Straight from
`media-audit:thumb_cache_path()`, except that the hash is a plain
non-cryptographic one — it names a cache entry, and md5 would be ceremony.

Generation is off-thread and the UI never blocks on it.

⟨designed⟩ `⌃G`/`⌃⇧G` re-seek ±10 s to pick a better frame. Not built, and
still wanted: the star rating is often a judgement about *this specific clip*,
so being stuck with whatever is 2 s in matters more than it sounds.

Disable with `--no-thumbnail`.

---

## 9. The write path

### 9.1 Plan before act

`w` builds a `WritePlan` and shows it for confirmation:

```
Write 3 files

  Genre       →  Karaoke              (all 3)
  Tags        →  #pov #hd #anal       (all 3)
  Rating      →  ★★★★☆                (1 file; 2 unchanged)
  Description →  removed              (all 3)

  faststart on · mdta

  ⏎ write   esc cancel
```

Nothing before this point touches disk. `mp4-tui-tagger`'s staging model,
kept — it is the reason that script is trustworthy. The plan also names the
backend it chose per file (§9.2), since "remux" and "in place" have very
different costs on a 6 GB file, and a plan that hides which one it picked is
hiding the only number the user might want to act on.

⟨designed⟩ `e` to edit the plan is not built; `esc` and re-editing the form is
the whole of it.

### 9.2 Choosing a backend — from the file, not from a flag

There are two writers, and the choice between them is a **correctness** decision
the tool makes, never a preference the user expresses:

```
                    ┌─ adding a key the file does not have?
                    │        (in place writes it unreadably — CONTAINER §3.2)
          ┌─────────┴─────────┐
        no│                   │yes
          ▼                   ▼
   carries XMP?          carries XMP?
    ┌─────┴─────┐         ┌────┴────┐
  yes│          │no     no│         │yes
    ▼           ▼         ▼          ▼
 exiftool   exiftool    ffmpeg    TWO-PASS
 in place   in place    remux     remux, then re-apply
 (must —    (cheap,               the XMP snapshot
 remux      keeps inode           taken at read time
 destroys   + xattrs)
 XMP)
```

Two measured facts drive every branch: a remux destroys XMP (§2.2), and an
in-place write cannot *add* an mdta key — exiftool writes it, exiftool reads it
back, and ffprobe cannot see it (docs/CONTAINER.md §3.2). Updating a key that is
already present is safe in place; adding one is not.

The two-pass case is not exotic — it is what happens the first time you set
Genre on a footage clip that has XMP but no `genre` atom. It works only because
both readers always run, so the XMP snapshot exists before the remux eats it.

All three branches are built — `Writer::Exiftool`, `Writer::Ffmpeg`,
`Writer::TwoPass` in `tags/plan.rs`, chosen from the file and executed in
`tags/write.rs`.

⟨designed⟩ **`--writer` and `--force` do not exist**, and their absence is not
a gap. The choice is a correctness decision, and every flag that lets a user
override it is a flag that lets a user destroy XMP. If a debugging override is
ever needed it should be spelled so that it cannot be typed by accident; until
something actually needs to debug a backend choice, no flag at all is the
stronger position.

#### 9.2.1 The remux

Per file, sequentially (parallel ffmpeg on one volume is slower, not faster):

```
ffmpeg -hide_banner -loglevel error -nostdin -y \
  -i FILE -map 0 -c copy -map_metadata 0 \
  [-movflags "+faststart+use_metadata_tags"] \
  -metadata major_brand= -metadata minor_version= \
  -metadata compatible_brands= -metadata encoder= \
  -metadata KEY=VALUE ... \
  -- TMP
```

- `-map 0 -c copy -map_metadata 0` — every stream, no re-encode, stream-level
  tags preserved. A metadata edit must never be lossy.
- the four empty `-metadata` assignments clear the muxer bookkeeping
  `use_metadata_tags` would otherwise promote to real tags (§2.1). Without them
  every rewrite accumulates them.
- `-metadata KEY=` (empty value) is how a key is *deleted*.
- `TMP` is `mktemp` in the **same directory** so the swap is a rename, and
  carries the source extension so ffmpeg picks the right muxer mode (§2.1).
- faststart costs a second pass over the temp file, not a second copy.

Then, before anything is replaced — `mp4doctor`'s discipline, which exists
because this library is 200 MB–6 GB files often on network volumes:

1. **Free space check first.** Need `size + 64 MiB`; a 6 GB file on a volume
   with 4 GB free fails *before* the ffmpeg run, with its own exit code and a
   message that says "not enough space", not "could not write tags".
2. **Verify duration** carried over (±5 s).
3. **Verify the tags read back** — re-probe the temp and diff against the plan.
   This is what catches a key ffmpeg silently dropped (§4.2), and it turns the
   mapping table from an assumption into something the tool checks every run.
4. **Verify faststart** if requested: parse the atom chain, `moov` before
   `mdat`. `mp4doctor`'s `atom_state()` in Rust, ~40 lines.
5. Restore mtime. ⟨designed⟩ macOS creation date is not restored — it is
   `setattrlist(2)` and nothing has yet missed it.
6. `rename(2)` over the original. ⟨designed⟩ There is no `FILE.backup.ext`
   option; see §5.8 for why it was dropped rather than deferred.

Any failure leaves the original untouched and the temp removed. The run
continues to the next file and the failure is reported in the summary — a bad
file in a batch of 40 must not abort the other 39.

### 9.3 The exiftool in-place path (milestone 4, not 8)

**Built.** Originally scoped as a late speed optimisation using `mp4ameta`.
Milestone 0 overturned both halves of that:

- **It is not a speed win on local storage.** Measured on a 475 MB fixture:
  ffmpeg remux **0.25 s**, exiftool in place **0.54 s**. On APFS the remux is
  memory-bandwidth work while exiftool pays ~0.2 s of Perl startup. A 6 GB local
  file remuxes in about 3 s. Whether it wins over SMB is *unmeasured*, and this
  document does not claim it does.
- **It is mandatory for correctness anyway**, because it is the only writer that
  preserves XMP (§2.2), the inode, and xattrs. That moves it from milestone 8 to
  milestone 4, and demotes speed to a side effect.

`mp4ameta` is dropped entirely — it covers only ilst, and the library is `mdta`.

Measured properties of `exiftool -overwrite_original_in_place`:

| Property | Result |
|---|---|
| inode | preserved — Finder tags and xattrs survive |
| atom chain | `ftyp moov free mdat` unchanged — faststart intact |
| unrelated keys | all preserved |
| tag growth (+8 KB) | absorbed by consuming the `wide`/`free` padding atom |

#### 9.3.1 Custom `Keys:` tags need a shipped exiftool config

Out of the box exiftool refuses them:

```
Warning: Sorry, Keys:Rating doesn't exist or isn't writable
Warning: Sorry, Keys:Actors doesn't exist or isn't writable
```

That is the exact wall `rename-footage` hit — hence its comment that "the atoms
have no equivalent for a rating, and ItemList:Keywords is not writable at all",
and hence its retreat to XMP. A user-defined config lifts it completely, and all
four custom keys then round-trip through ffprobe with the other 20 untouched.

`tagform` ships `assets/tagform.exiftool.cfg` declaring `actors`, `channel`,
`type`, `rating`, `origin`, `source_url` and `webpage_url` on
`QuickTime::Keys`, and passes `-config` on every exiftool invocation. It is a
**required runtime asset**, installed alongside the binary — not an optional
extra, and not something to regenerate at runtime.

#### 9.3.2 Fallbacks

If the moov cannot grow into the available padding, or exiftool exits non-zero,
the writer falls back to the remux — **unless** the file carries XMP, in which
case it fails loudly instead. Falling back to a writer known to destroy data is
not a fallback.

### 9.4 Filename sync

⟨designed⟩ **Not built.** No filename is parsed or composed anywhere in the
code today, which also means §3.6's field-precedence rule ("no edit, no
metadata → parse it out of the filename") is inert: an empty field stays empty.
This is the largest remaining piece of milestone 6, and the section below is
the design it should be built to.

When **Sync filename** is checked, the file is renamed — but to **one of two
grammars**, selected by Genre, because this library has two and they are close
to inverses of each other:

**Media** (`ytform`, `media-parse-filename-to-json`) — Genre is anything but
`Footage`:

```
Actor A, Actor B - [Channel] Title #tag1 #tag2 (Origin) ★★★☆☆.ext
```

**Footage** (`rename-footage`) — Genre is `Footage`:

```
YYYY-MM-DD--HH-MM-SS People (Channel) - Title Location #tags [1080p 30fps 4min h264 iPhone15Pro H].ext
```

Note how they invert: media puts Channel in `[...]` and Origin in `(...)`;
footage puts Channel in `(...)` and the probed spec block in `[...]`. Composing
one file with the other's grammar produces a name that the other parser reads
back **wrongly rather than not at all**, which is the worst possible failure. So
the grammar is chosen explicitly from Genre, the choice is shown in the write
plan, and `--grammar media|footage|auto` overrides it.

Rules shared by both, taken from `ytform`'s `compose.go` and `rename-footage` so
the three tools cannot disagree: `/` becomes `-`; newlines and tabs become
spaces; the stem is truncated to 240 bytes (`media-audit`'s
`MAX_FILENAME_BYTES`). Media-only: a Channel equal to the first Actor is
dropped, and a rating of 0 emits no stars. Footage-only: **the rating never
appears in the name** (it is embedded only), every segment drops out when empty,
the ` - ` appears only when something follows it, and the `[...]` block is
re-probed fresh rather than carried over.

Rename happens after the metadata write succeeds, never before. When a file is
renamed and carries no `PreservedFileName`, `tagform` stamps the pre-rename name
into it first (§3.6) — write-once, exactly as `rename-footage` does.

This is the one place `tagform` changes something outside the container, so it
is off by default and shown in the plan as an explicit line.

---

## 10. CLI surface

**Built.** The whole of it:

```
tagform [OPTIONS] FILE...

  --print-json     dump the aggregated tag model and exit
  --no-thumbnail   do not render a thumbnail
  --theme=NAME     colour scheme; `c` cycles them at runtime
  -h, --help       show this message
```

Exit codes: `0` success, `2` anything that went wrong. Argument parsing is a
hand-rolled loop over `std::env::args()` — no `clap`, because four options do
not need a parser and a dependency added early is one you never get to remove.

⟨designed⟩ The rest of the surface, in the order it is worth building:

```
Input
      --from-filename       seed empty fields from the filename grammar   (§9.4)
      --fetch               seed from yt-dlp using the embedded URL       (§5.5)
      --from-json FILE      seed from a yt-dlp .info.json
  -R, --recurse             expand directory arguments via fd-media

Output / mode
      --compat MODE         mdta | ilst | both                    [mdta]   (§2.1)
      --set KEY=VALUE       set a field non-interactively (repeatable)
      --unset KEY           clear a field (repeatable)
      --apply               with --set/--unset: write and exit, no TUI
      --dry-run             build and print the write plan, write nothing

Write
      --no-faststart        do not add +faststart to the remux    [on]
      --sync-filename       rename to the filename grammar                (§9.4)

Presentation
      --config PATH                                                       (§12)
```

Two of these have to be designed for rather than bolted on. `--dry-run` is
nearly free — the plan already exists as a value before anything touches disk
(§9.1), so it is a print instead of a confirm. `--apply` is not: a headless
mode **must never open `/dev/tty`**, and it is what makes `tagform` usable from
`.job` scripts and from `media-audit`'s fix path. It is a first-class surface,
not an afterthought, and it should get exit codes of its own — `1` for one or
more files failed, `3` for insufficient disk space — since a script that cannot
tell "no space" from "bad arguments" will retry the one thing that can never
work.

Dropped rather than deferred: `--mouse` (§6.3), `--edit-custom` (§3.7),
`--backup` (§5.8), `--writer` / `--force` (§9.2), `--fast` (the in-place path
is chosen for correctness, never for speed — §9.3).

---

## 11. Keys

**Built, and modal.** The original design assumed one mode with every field
live, which forced every command onto a modifier. A Select/Edit split is
better: `j`/`k` move and `enter` opens a field, so single-letter commands are
free — `w` can mean write precisely because nothing is listening for the letter
`w` until you press `enter`.

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
| `backspace` | clear the focused field (staged like any edit; `u` undoes it) |
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

On a text field, the emacs/macOS editing keys as well — `⌃A` `⌃E` `⌃B` `⌃F`
`⌃D` `⌃H` `⌃W` `⌃K` `⌃U`, table in §5.1. They bind only while a field is open,
which is exactly why the mode split makes them affordable.

⟨designed⟩ Unbound, each waiting on its feature: `⌃Space` (completion, §5.1),
`⌃O` / `⌃Y` (open / yank), `⌃G` / `⌃⇧G` (thumbnail seek, §8), `?` (key help —
the shortcut strip and `--help` cover it for now).

⟨designed⟩ Two features have **lost the key they were reserved for**, since
the editing bindings have the stronger claim on a text field: the `$EDITOR`
escape hatch wanted `⌃E` (§5.2) and fetch wanted `⌃F` (§5.5). Both need a new
one before they are built. `⌃X` is free and unclaimed by anything in §5.1,
which makes an `⌃X`-prefixed pair the obvious answer if a third feature ever
wants a key too.

---

## 12. Config

⟨designed⟩ **There is no config file.** `config.rs` reads exactly one thing:
`~/.config/yt-dlp/config`, for the Genre and Type aliases (§3.5). Everything
else — theme, faststart, enums, defaults, field order — is either a flag, a
runtime toggle, or a constant.

That is a better position than it looks, and it should be defended until
something specific breaks it. Four of the seven proposed tables have no
customer: `[fields].order` and `.hidden` matter only once the form is long
enough to need curating, `[defaults]` writes values nobody asked for, and
`[enums]` duplicates a file that is already the source of truth. A config key
is a permanent compatibility obligation bought for a one-line default.

The one table with a real customer is `[completion].history`, and it comes with
the feature that needs it:

```toml
[completion]
history = 500        # values remembered per field
```

Value history is data, not config, and belongs in
`~/.local/share/tagform/values.json`. When the config file does land, `--config
PATH` lands with it, and the deployment recipe is this repository's own
(`cargo build --release`) rather than the dotfiles monorepo's `dotter` entry
that this section originally assumed.

---

## 13. Crate layout

**Built.** The tree, as it stands:

```
tagform/
├── SPEC.md                       # this document
├── README.md                     # what runs today
├── AGENTS.md                     # orientation for coding agents (CLAUDE.md links here)
├── Cargo.toml
├── docs/CONTAINER.md             # milestone 0's findings — measured, done
├── assets/tagform.exiftool.cfg   # required runtime asset (§9.3.1)
├── tests/
│   ├── container-experiment.sh   # regenerates CONTAINER.md's numbers
│   └── write-paths.sh            # exercises the three write backends
└── src/
    ├── main.rs                   # CLI, --print-json report, exit codes
    ├── config.rs                 # the yt-dlp alias parse (§3.5)
    ├── thumb.rs                  # extraction, cache, ratatui-image (§8)
    ├── model/
    │   ├── schema.rs             # FIELDS — the field/key table (§3)
    │   └── value.rs              # Value, Agg (§4.3)
    ├── tags/
    │   ├── probe.rs              # ffprobe + exiftool → FileTags (§4.1)
    │   ├── atoms.rs              # atom-chain parse, faststart (§9.2.1)
    │   ├── plan.rs               # what to write, and which backend (§9.1, §9.2)
    │   └── write.rs              # executing a plan: remux, verify, rename (§9.2.1)
    └── ui/
        ├── app.rs                # event loop, modal state, focus, undo (§6.3, §6.4)
        ├── edit.rs               # the Editor enum — every control (§5)
        ├── render.rs             # painting (§7)
        └── theme.rs              # four schemes + the contrast test (§7)
```

⟨built, differs⟩ Flatter than planned, in three places. `tags/ffmpeg.rs`,
`tags/exiftool.rs` and `tags/xmp.rs` are one `write.rs`, because the three
backends share the verify-then-rename spine and splitting them would have
duplicated it. `ui/controls/` — twelve files behind a trait — is one
`edit.rs` holding one enum (§5). `model/form.rs`, `model/filename/` and
`seed/` do not exist: the first became state on `App`, the other two are
unbuilt features (§9.4, §10).

Everything outside `ui/` is pure enough to unit test without a terminal, which
is the point of the split and the one structural rule worth keeping.

---

## 14. Testing

No CI, so tests have to be worth running by hand.

**Built** — `cargo test`, no fixtures, tests live in-file under `#[cfg(test)]`:

- `ui/edit.rs` — the largest suite: control behaviour, the URL validation table
  in §5.5, date normalisation, list and hashtag splitting
- `ui/theme.rs` — WCAG contrast for every colour in every scheme, and the
  custom-label hue check (§7)
- `ui/app.rs` — aggregation, staging, merge
- `tags/probe.rs`, `tags/atoms.rs` — parsing, faststart detection
- `config.rs` — the yt-dlp alias parse
- `thumb.rs` — the cache key, and the `decrease`-not-`increase` assertion (§8)

**Container** — `tests/container-experiment.sh` regenerates every number in
[docs/CONTAINER.md](docs/CONTAINER.md). Run it after any ffmpeg or exiftool
upgrade; the findings are version-specific.

**Write paths** — `tests/write-paths.sh` exercises the three backends against
real files. Both scripts mutate real media, which is why neither runs from
`cargo test`.

⟨designed⟩ **The gap that matters is a fixture suite.** There is no
`tests/fixtures/`, no generated tiny.mp4, and so no test that a write
round-trips without touching real media:

```bash
ffmpeg -f lavfi -i testsrc=d=2:s=320x240 -f lavfi -i sine=d=2 \
  -c:v h264 -c:a aac tests/fixtures/tiny.mp4
```

In rough order of what they would have caught:

- **the XMP regression** — write XMP with exiftool, run a `tagform` write,
  assert every XMP field survives. This is the test for the §2.2 data loss, and
  the one the whole write path's design rests on being true.
- **backend selection** — assert a file carrying XMP never routes to a bare
  remux.
- **round-trip every field** — write, re-probe, assert every key comes back.
  This is what turns §3's table from an assumption into something checked.
- delete semantics: `-metadata key=` removes rather than empties.
- stream tags and a second audio track survive the remux.
- faststart: a deliberately moov-at-end fixture, fixed and detected.
- failure paths: read-only file, no space, a truncated input.

**Manual**: kitty / Ghostty / iTerm2 / tmux / plain xterm-256color for the
thumbnail ladder, and one run over SMB for the timing story in §9.3.

---

## 15. Integration

`tagform` is now its own repository
([monomadic/tagform](https://github.com/monomadic/tagform)), built with
`cargo build --release` and installed to `~/.local/bin/tagform`. That is a
change of direction from this document's original assumption, which was a
directory inside the `~/config` dotfiles monorepo with a `setup/install/`
script, a `config/tagform/` deployment and a `dotter/global.toml` entry.

The split is worth stating as an intent rather than an accident: `tagform` has
two hard runtime dependencies (`ffmpeg`, `exiftool`) and one soft one (the
yt-dlp config it reads its enums from). Nothing else ties it to that monorepo,
and the tool is more useful to anyone else as a repository than as a
subdirectory of someone's dotfiles.

⟨designed⟩ The integrations back into the monorepo's media toolchain are
unbuilt, and each needs something first:

- `media-audit` gains `t` on the metadata issue screen → `tagform --fetch FILE`,
  replacing the current fetch-or-skip prompt with an editable form. Needs
  `--fetch` (§5.5).
- `fzf-media-select` / `ls-media` bind a key to `tagform {+}` — multi-select
  passes straight through as file arguments. Needs the `/dev/tty` handling in
  §6.3, or it cannot run inside fzf at all.
- `mp4-tui-tagger` keeps a deprecation banner pointing at `tagform` once
  `tagform` covers the custom-key editing it does — which it now does (§3.7).
  It is not deleted in the same change that lands the replacement.

---

## 16. Milestones

| # | Deliverable | |
|---|---|---|
| **0** | [docs/CONTAINER.md](docs/CONTAINER.md). Settled open question 1, killed `mp4ameta`, promoted the exiftool writer from an optimisation to a correctness requirement. | ✅ |
| **1** | Probe (ffprobe **+ exiftool**) → model → aggregate → `--print-json`. No UI. | ✅ |
| **2** | Read-only TUI: layout, focus ring, thumbnails, inspector. | ✅ |
| **3** | The controls + validation + undo. Still no writes. | ✅ |
| **4** | Both writers, backend selection (§9.2), verification, atomic swap, faststart. Feature-complete for one file. | ✅ |
| **5** | Multi-file: merge, per-file inspector, batch summary, partial-failure reporting. | ✅ |
| **6** | The Footage profile: XMP fields ✅, `PreservedFileName` ✅, the second filename grammar ⬜ (§9.4). | ◐ |
| **7** | Seeding: `--from-filename`, `--fetch`, completion history. | ⬜ |
| **8** | Headless `--set`/`--apply`, the remaining §3.2 fields, config file, `--compat both`. | ⬜ |

**Where this actually stands.** Milestone 6 turned out to be two independent
halves, and the important one is done: XMP is read, written, and restored
across a remux, so the data loss this whole design was built to prevent cannot
happen. Which means the note this section used to carry — *"until milestone 6
lands, `tagform` must refuse any file carrying XMP"* — is **discharged**. It
does not refuse them; it routes them to the writer that preserves them (§9.2),
and verifies the XMP read back afterwards.

**The direction from here** is narrower than the original 8-milestone plan, and
deliberately so. What is left divides into three:

1. **Filename work** (the rest of 6) — the two grammars, parsing as well as
   composing. This is the last piece that unlocks something the old scripts
   could do and `tagform` cannot.
2. **Seeding** (7) — `--fetch` and completion. The largest ergonomic wins
   available, and both are additive: nothing about the existing form changes.
3. **A fixture suite** (§14) — not a milestone in the original plan, and it
   should have been. It is the cheapest remaining risk reduction in the
   project, and it gates being comfortable with any of the above.

Milestone 8's contents have thinned. Headless `--apply` is real work with a
real customer (§10). The remaining §3.2 fields, the config file and
`--compat both` are all speculative, and each has a note above explaining why
building it now would be cost without a customer. **The default answer to
"should this be added" is no until a file, a script, or a person needs it** —
which is the discipline that kept the form to nineteen rows and the CLI to four
flags.

---

## 17. Open questions

1. ~~Does ffmpeg write `mdir` and `mdta` together?~~ **Settled: no.** One box or
   the other (§2.1). `--compat both` needs the exiftool second pass.
2. ~~Where should the star rating live?~~ **Settled in practice.**
   `XMP-xmp:Rating` is a real standard 0–5 field and is used wherever XMP is
   present; elsewhere the `rating` key holds it. The `com.apple.iTunes:rating`
   freeform atom was never built, and the fallback position — "stars are a
   filename-and-XMP concept, not an ilst one" — is now simply the design. It
   reopens only if `--compat ilst` is ever built and something needs Plex to
   see a rating.
3. **Is the exiftool path actually faster over SMB?** Still unmeasured, and
   still does not matter: it is chosen for XMP, inode and xattr preservation,
   never for speed (§9.3), and there is no `--fast` flag for it to inform. One
   measurement on the Tower volume would settle it; nothing is blocked on it.
4. **Should Artist auto-mirror Actors?** Unbuilt, and looking less likely.
   The yt-dlp config writes the same value to both, so the sketch in §7 showed
   Artist dimmed with `(auto)`, breaking the link on first edit. In a form
   where every other field means exactly what it shows, one field that quietly
   follows another is the kind of magic that costs more in surprise than it
   saves in typing.
5. **`iTunMOVI`** — the plist blob holding cast, directors, producers and
   studio — is the only way Apple software sees an actor list. Still deferred,
   and it is the thing to build if `--compat ilst` ever gets a customer:
   without it, an ilst write produces a file whose cast Apple cannot read,
   which is most of the reason to have written ilst at all.
6. **What actually triggers the ffprobe blind spot?** Unresolved. Not value
   size — that first guess was disproved on re-run. The one reproducible case
   is a 475 MB `.mov` whose `wide` padding atom was consumed by an 8 KB write
   (docs/CONTAINER.md §4). The writer now guards the *symptom* — it never
   writes an empty value over a field whose two readers disagree about
   emptiness (§4.2) — so this is no longer urgent, but the cause is still
   unknown and the in-place writer meets consumed padding routinely.
7. **Should `~/.config/yt-dlp/config` change `Camera Footage` to `Footage`?**
   The normalisation in `config.rs` (§3.5) makes `tagform` correct either way,
   so this is the user's call and affects only new downloads. One line, in
   another repository, not made as part of this work.
