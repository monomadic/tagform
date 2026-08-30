# AGENTS.md

Guidance for coding agents working on `tagform`. `CLAUDE.md` is a symlink to
this file.

`tagform` is a Rust TUI that edits metadata on MP4/MOV files by shelling out to
`ffmpeg`/`ffprobe` and `exiftool`. ~4,700 lines across 16 source files.

## Read this before reading anything else

The docs in this repo are large and mostly *reference*. Reading them end to end
is the main way to waste a context window here. Budget them like this:

| File | Lines | How to use it |
|---|---|---|
| `README.md` | 135 | **Read in full, once.** Status, keymap, and the three container facts the design rests on. |
| `SPEC.md` | 1498 | **Never read whole.** Section-scoped: `grep -n '^## ' SPEC.md`, then `sed -n 'A,Bp'`. |
| `docs/CONTAINER.md` | 253 | Read §1 only, and only when touching the write path. Measured ffmpeg/exiftool behaviour. |

`SPEC.md` section starts, so you can jump without grepping first:

```
26 §1 what this is · 75 §2 container problem · 174 §3 schema · 435 §4 keys
538 §5 controls · 724 §6 tui choice · 837 §7 layout · 893 §8 thumbnails
941 §9 write path · 1159 §10 cli · 1216 §11 keys · 1260 §12 config
1290 §13 crate layout · 1337 §14 testing · 1388 §15 integration
1418 §16 milestones · 1462 §17 open questions
```

**SPEC.md marks what is real.** It is a design document written ahead of the
code, so it carries two markers: **`⟨designed⟩`** means not built — intent, not
behaviour — and **`⟨built, differs⟩`** means built, but not the way that
section first described it, with a note on how. Unmarked text describes the
code. Do not implement against a `⟨designed⟩` block without checking whether
the reason it is unbuilt is written next to it; several are deliberate
rejections rather than a backlog. §16 has the current direction.

Use the map below instead of searching the tree.

## Where things are

```
src/main.rs         CLI, --print-json report, exit codes
src/config.rs       config.toml + the yt-dlp --alias parse (Genre/Type sets)
src/thumb.rs        thumbnail extraction, cache, ratatui-image
src/model/
  schema.rs         FIELDS table — field → mdta/read/xmp/ilst keys. Start here.
  value.rs          Value, Agg (the Same/Mixed aggregation across files)
src/tags/
  probe.rs          ffprobe + exiftool → FileTags
  atoms.rs          atom-chain parse, faststart/Layout detection
  plan.rs           what to write and which backend writes it. Touches no disk.
  write.rs          executes a plan. The remux, the verify, the rename.
src/ui/
  app.rs            event loop, modal state, focus ring, undo/redo (939 lines)
  edit.rs           per-control edit behaviour
  render.rs         painting
  theme.rs          four schemes + the WCAG contrast test
```

Field behaviour is nearly always a `schema.rs` question first. A field is one
label and one control, but fans out to *many* container keys — the URL field
writes five. That fan-out is the reason the tool exists.

## Commands

```bash
cargo test                      # unit tests live in-file under #[cfg(test)]
cargo run -- FILE...            # the form
cargo run -- --print-json FILE... # model as JSON — the fastest way to inspect a real file
cargo build --release
```

There is no integration-test harness and no CI. `tests/` holds two shell
scripts that mutate real files — `container-experiment.sh` regenerates
`docs/CONTAINER.md`'s numbers, `write-paths.sh` exercises the write path.
**Do not run them without asking**; they need real media and rewrite it.

`--print-json` is the cheap oracle. Prefer it over launching the TUI, which you
cannot drive from a non-interactive shell anyway.

## Invariants — do not rediscover these by breaking them

These were established by measurement (`docs/CONTAINER.md`) and cost real
debugging. Changing code that violates one is a regression, not a refactor.

1. **This library is `mdta`, not iTunes.** Tags live in `moov/udta/meta` under
   the `mdta` handler. The default ffmpeg path writes `ilst` atoms and silently
   drops 9 of 20 keys. The two layouts are mutually exclusive.
2. **An ffmpeg remux destroys XMP, silently, with no flag to prevent it.** The
   writer therefore picks its backend from *file contents*, not user
   preference — never from a flag. (SPEC's `--writer ffmpeg` / `--force`
   escape hatch is designed but not implemented; `main.rs` accepts only
   `--print-json`, `--theme`, `--no-thumbnail`, `--help`.)
3. **The original is never modified until a verified replacement exists.**
   `write.rs` remuxes to a sibling temp, proves duration, tags and layout, and
   only then renames over the original. Any failure leaves the original
   untouched. Do not add a path that edits in place without that proof.
4. **Unrecognised keys are never dropped.** Keys no field claims are carried in
   `Report::custom` and preserved through a write.
5. **Read is deliberately wider than write.** A URL may arrive as `comment`,
   `purl`, `source_url`, `webpage_url` or `original_url`; write emits only the
   canonical set. That asymmetry is what makes `tagform` idempotent — keep it.
6. **`assets/tagform.exiftool.cfg` is a required runtime asset.** Without it
   exiftool refuses to write the custom `Keys:` tags. A key missing from
   `EXIFTOOL_KEY_NAMES` in `plan.rs` cannot be updated in place and forces a
   full remux.
7. **Colours are guarded by a test.** `theme.rs` fails below 3:1 WCAG contrast
   and requires custom-key labels to differ in *hue*, not brightness. Both
   guards exist because both mistakes were already made.

## Conventions

- Module-level `//!` docs carry the *why* and cite the SPEC section. Match that
  when adding a module; comments here explain decisions, not mechanics.
- Everything outside `ui/` is testable without a terminal. Keep that split —
  it is the reason the test suite exists at all.
- `anyhow` throughout; errors print as `tagform: {e:#}` and exit 2.
- External tools are invoked via `std::process::Command` with `--` before
  paths. No shell interpolation anywhere.

## Working economically here

- **Don't re-derive container behaviour.** If a question is "what does ffmpeg
  actually do to X", it is answered in `docs/CONTAINER.md`. Ask the doc, not a
  test file.
- **Don't grep the repo for a field.** `FIELDS` in `schema.rs` is the index.
- **Don't read `SPEC.md` to learn the current state** — it describes the
  intended design across all 8 milestones; the README's Status section says
  where the code actually is (milestone 5 of 8: multi-file).
- **Don't spawn a subagent to explore this tree.** It is 16 files and mapped
  above; the round trip costs more than reading the file you need.
- Prefer `--print-json` over reasoning about what a file contains.
