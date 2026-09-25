# TASKS.md — write-path safety work

Derived from `AUDIT_HANDOFF.md` (2026-09-03), re-checked against the tree on
2026-09-25. Every code-level finding in the audit still reproduces; the write
path (`src/tags/write.rs`, `native.rs`, `probe.rs`, `plan.rs`) has had only
feature work since. The `clone` subcommand (2026-09-19) is a second, headless
entry into that path with no confirmation dialog, which raises the cost of
every fail-open below.

Items are in execution order. 1–6 together are roughly a day and close every
genuine fail-open. Tick a box, cite the commit.

## Do

- [x] **1. Fail closed in `probe_xmp`.** `src/tags/probe.rs` returns an empty
  map whenever exiftool prints nothing, without checking the exit status. A
  failing exiftool (missing config, unreadable file, wrong binary) reads as
  "no XMP" and the planner may pick the remux that destroys XMP — invariant 2
  in spirit. Accept only the known no-XMP outcome; surface everything else.
  Fixture: point at a broken exiftool path and assert an error, not an empty
  map. Ten lines. Do this first.

- [x] **2. Fix `restore_mtime`, verify XMP on the in-place path.**
  `restore_mtime` in `write.rs` runs `touch -r path path` and discards the
  captured time, so every write bumps mtime. Use the `filetime` crate.
  `in_place` calls only `verify_atoms`; add `verify_xmp` when `plan.xmp` is
  non-empty. Fixtures: mtime survives a native write; an XMP-only in-place
  write is verified.

- [ ] **3. Fingerprint files at probe time.** The form now stays open after a
  write and `rename-video`, yt-dlp, `clone` and the user's shell can all touch
  a file between probe and write. A stale XMP snapshot in the two-pass path can
  overwrite newer XMP. Record `(dev, ino, size, mtime)` on `FileTags`, check
  in every writer entry point, refuse with "file changed since it was opened;
  re-probe". Fixture: probe, alter externally, execute the old plan, assert
  refusal and an untouched file. No content hashing — overkill here.

- [ ] **4. Narrow `native::fold` to refuse conflicting keys.** `fold` merges
  every mdta `meta` box into the first by name and drops the rest. Keep that
  for the measured exiftool split-key debris (docs/CONTAINER.md §8). Decline
  native writing when the same key carries *different payloads* across boxes,
  and let the planner fall back. While there, make `Entry` retain the `keys`
  namespace and refuse non-`mdta` or duplicate entries rather than
  normalising them (audit item 5, folded in). Fixture: synthetic two-box
  file with conflicting values declines and is untouched; the debris fixture
  still repairs.

- [ ] **5. Plan-time Actors/Artist conflict error.** Both fields write the
  `artist` key (`schema.rs`). A staged Actors and a staged Artist with
  different values currently fail late in verification after a writer has
  run. Detect by destination key in `plan.rs` before the confirmation dialog
  and show a direct error. Product rule is settled — DESIGN §17.4 rejected
  auto-mirroring; the fields stay independent.

- [ ] **6. Unpredictable temp name, fsync before rename.** `temp_beside` uses
  the PID; use a random suffix and create with `create_new`. `sync_all` the
  verified temp, then fsync the directory, before `rename`. Closes the last
  gap in invariant 3 for the native and ffmpeg paths.

- [ ] **7. Clippy, fmt, CI.** *(clippy and fmt green as of 2026-09-25; CI still to add.)* `cargo clippy --all-targets -- -D warnings`
  reported 5 errors at audit time and 14 today; `cargo fmt --check` fails.
  Fix, then add a workflow running `fmt --check`, clippy and `cargo test`
  with ffmpeg and exiftool installed (the fixture suite needs both). This
  item is cheap and stops the count compounding.

- [ ] **8. Document the in-place residual risk.** The exiftool in-place path
  is kept deliberately (DESIGN §9.3: it preserves inode, xattrs, Finder
  tags). exiftool writes its own temp and copies back only on success, so
  the exposed window is the copy-back. Record that as an accepted risk in
  §9.3, and reword CLAUDE.md invariant 3 to say why this path is the
  exception rather than contradicting it.

## Deliberately not doing

Recorded so the next audit does not re-raise them.

- **Audit item 1, transactional exiftool writes.** Copy-then-rename discards
  the inode and xattrs that are the whole reason the path exists. See item 8.
- **Audit item 3 beyond mtime and mode bits.** ACL, xattr, quarantine and
  creation-time restoration for the native/ffmpeg paths is second-tier.
  Revisit only if the native writer cannot later be made in-place itself.
- **Audit item 6, before/after manifests.** The native writer copies untouched
  boxes byte-for-byte. Defer until a real regression motivates it.
- **Audit item 11, real-media matrix in CI.** `mebx`, `gpmd` and >4 GiB cases
  stay in the documented local harness (`tests/write-paths.sh`), which needs
  real media and must not run unasked.
- **Replacing ffmpeg/exiftool with FFI.** Agreed with the audit's non-goal.
