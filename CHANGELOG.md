# Changelog

What changed for someone *using* `tagform`. Newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); how to maintain it is
in `AGENTS.md`.

## [Unreleased]

### Added

- `M` makes a Matroska copy of the file in view (or every open file) beside
  it, in the background. The source is kept; a file holding something
  Matroska cannot carry is refused, with the list.
- **Matroska support.** `.mkv` files are read and written through `fastmkv`,
  recognised by their contents rather than their extension.
- `tagform convert FILE...` — rewrite an MP4 or MOV as a Matroska file,
  headless. Refuses what Matroska cannot carry.
- Filename composition from tags for Matroska files.
- The header shows the container type, and colours each fact in its tech line.
- `⌘U` loads every video beside the current file as a fresh batch.
- Track number is inferred from a batch of clip filenames.
- `tagform clone SRC DST...` — copy one file's tags onto others, headless.
- **Place lookup.** Typing into the Place row (or `i l`) looks the place up
  with MapKit and fills Location, State, Country and Coordinates. `⏎` on a
  filled Coordinates row names the place at them.
- **Background write queue.** Writes run in the background while the form
  stays live; the header panel shows the queue, with bars weighted by file
  size.
- Renaming a file from its tags is a field on the write, run after it lands.
- A navigable import menu with source selection.
- Track number can be typed straight from Select mode.
- `d` on the URL field fetches tags from the page with `yt-dlp`, without
  downloading.
- Hashtag grammar for tags, with repair and validation.
- The adult profile, with its Orientation field; the `@G`/`@L`/`@T` mark is
  read back from a filename.
- The Track field, bulk-mode notes, a save hotkey, and exit on write.
- Every enum is drawn as a picker; Footage has its own form.
- Case cycling in text fields.

### Changed

- The faststart switch is shown at the top right, beside `?  help`, rather than at the end of the mode bar.
- Tags sort alphabetically, ignoring case.
- A mixed set counts its answers instead of only saying "mixed".
- Ratings behave as fixed sets.
- Write status sits bottom right; the write plan is one line.
- Focus is easier to see.
- A missing external tool is named in the error, with where it was looked for.
