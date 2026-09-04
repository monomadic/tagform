# tagform audit handoff — 2026-09-03

## Scope and current result

This is a read-only follow-up audit of the current `tagform` worktree. It
includes the native `mdta` writer, generated fixture suite, write-progress
work, and batch-staging changes.

Verification run:

- `cargo test`: **149 passed, 1 ignored**. The fixture suite generates and
  mutates only temporary media files; the two real-media shell scripts in
  `tests/` were not run.
- `cargo clippy --all-targets --all-features -- -D warnings`: fails on five
  minor lint issues.
- `cargo fmt --check`: fails.

The native writer is the right direction. It eliminates the common
add-a-key → ffmpeg-remux path and therefore preserves XMP and unknown tracks
that a remux may lose. It should, however, remain deliberately conservative:
when a container's metadata topology is ambiguous, decline it rather than
repair it by assumption.

## What has improved

- `tags/native.rs` rebuilds `mdta` `keys` and `ilst` together, correcting
  ExifTool's key/item split behavior.
- Native writes use a sibling temporary output, verify it, then rename it over
  the source.
- Generated fixtures exercise XMP survival, field round-trips, custom-tag
  preservation, deletion, faststart, extra audio/timecode tracks, malformed
  files, fragmented-file decline, and a failed replacement.
- The Actors/Artist shared-key conflict now fails without replacing the
  original, rather than silently choosing one value.
- Failed batch writes retain unlanded staged edits.

## Priority 0 — restore the claimed write-safety invariant

### 1. Make every write transactional, including ExifTool updates

**Risk:** high. `Writer::Exiftool` invokes ExifTool with
`-overwrite_original_in_place`, and only verifies afterward. If writing or
verification fails, the source may already be modified. XMP-only writes are
not verified at all because this path calls only `verify_atoms`.

Relevant code:

- `src/tags/write.rs`: `in_place`, especially the ExifTool invocation and the
  final `verify_atoms` call.
- `src/tags/plan.rs`: `Writer::Exiftool` selection.

**Recommended task:** replace the in-place mutation path with a temp-copy
workflow:

1. Create a secure sibling temporary path.
2. Copy or clone the source to that path.
3. Run ExifTool against the temp.
4. Verify requested atoms and XMP on the temp.
5. Restore filesystem metadata (see task 3).
6. Atomically rename the temp over the source.

If retaining in-place writes temporarily, at minimum call `verify_xmp` when
`plan.xmp` is non-empty; this improves reporting but does **not** solve the
rollback problem.

**Acceptance tests:** force ExifTool/verification failure after an attempted
update and assert byte-identical source contents; add a successful XMP-only
write fixture.

### 2. Refuse files changed since they were probed

**Risk:** high. Plans and XMP snapshots are made when the form opens, but the
write happens later, on a worker thread. Another process can change the file
between probe and execution. A stale XMP snapshot is especially dangerous in
the two-pass fallback.

Relevant code:

- `src/tags/probe.rs`: `FileTags` has no source identity/fingerprint.
- `src/ui/app.rs`: `apply` passes snapshots captured from the old model.
- `src/tags/write.rs`: all writer entry points accept the stale plan.

**Recommended task:** record a fingerprint at probe time and check it before
writing. Start with device/inode, size, and modified time; for strong safety,
also hash the relevant `moov`/XMP extents or capture a stable content digest.
Reject a mismatch with a clear “file changed since it was opened; re-probe and
review again” error.

**Acceptance tests:** probe a fixture, alter it externally, then execute its
old plan and assert refusal with no further changes.

### 3. Preserve source filesystem metadata across replacement

**Risk:** high. Native and ffmpeg writers replace the source with a new temp
file. They do not preserve permission bits, ownership, ACLs, Finder tags,
extended attributes, quarantine state, creation time, or timestamps.

Additionally, `restore_mtime` is currently a no-op: it executes
`touch -r path path` and ignores the captured timestamp.

Relevant code:

- `src/tags/write.rs`: `swap`, `mtime`, `restore_mtime`.

**Recommended task:** introduce a small `FileMetadataSnapshot` captured before
creating the temp and restored onto the verified temp before rename. On macOS,
cover mode, uid/gid when allowed, timestamps, xattrs, and ACLs. Be explicit
about metadata that cannot safely be restored without elevated permission.
Use a Rust API such as `filetime`/platform APIs for mtime rather than shelling
out to `touch`.

**Acceptance tests:** on macOS, assert mode, selected test xattr, and mtime
survive a native write; add portable mode/mtime coverage where possible.

## Priority 1 — make the native rewriter fail closed

### 4. Decline ambiguous multi-`mdta` metadata layouts

**Risk:** medium-high. `native::fold` merges every discovered mdta `meta` box
into the first and removes the rest. This repairs the measured ExifTool debris
case, but assumes secondary boxes are always debris. A legitimate second box
can have conflicting values, and box order is not a sufficient ownership rule.

Relevant code:

- `src/tags/native.rs`: `fold`, `Survey::absorb`, and `rewrite`.

**Recommended task:** narrowly recognize and repair only the observed
split-key/stray-item pattern. For independently complete boxes, duplicate keys
with different payloads, or unknown topology, decline native writing and use a
safe fallback (or refuse if no fallback is safe).

**Acceptance tests:** synthetic multi-meta fixtures with conflicting values
must decline and leave the source untouched; the known ExifTool debris fixture
may still be repaired.

### 5. Validate the full `keys`/`ilst` shape before rebuilding it

**Risk:** medium. The native parser ignores each `keys` entry's namespace and
always writes `mdta` entries. It should not rewrite a box it cannot completely
understand. Duplicate names, unexpected namespaces, duplicate `keys`/`ilst`
boxes, or malformed trailing bytes should be refusal conditions.

Relevant code:

- `src/tags/native.rs`: `read_meta`, `pair`, `build_keys`, `build_ilst`.

**Recommended task:** make `Entry` retain/validate namespace, require `mdta`
where that is a writer precondition, reject duplicate or ambiguous entries, and
ensure parsed child boxes are consumed exactly. Prefer a targeted refusal over
normalizing unknown data into a different layout.

### 6. Verify preservation, not only requested edits

**Risk:** medium. Runtime verification checks duration, a coarse stream tally,
and planned atom changes. It does not compare untouched custom atoms, untouched
XMP, chapters, stream language/disposition/codec parameters, or filesystem
metadata.

Relevant code:

- `src/tags/write.rs`: `verify_atoms`, `verify_xmp`, `verify_streams`.

**Recommended task:** derive a before/after manifest. Require equality for all
unmodified atom/XMP values, plus a richer stream manifest. Continue allowing
only expected planned differences. Consider raw preservation checks for native
copied boxes where feasible.

## Priority 2 — remove remaining fail-open and late-failure behavior

### 7. Treat ExifTool read failures as errors, not absent XMP

**Risk:** medium. `probe_xmp` returns an empty map whenever ExifTool produces
empty stdout, without checking whether the process failed. This can incorrectly
select a writer as though no XMP exists.

Relevant code:

- `src/tags/probe.rs`: `probe_xmp`.

**Recommended task:** inspect ExifTool status and only accept the known
no-XMP outcome. Surface every other process/read/parsing failure.

### 8. Detect shared destination-key conflicts while planning

**Risk:** medium. Actors and Artist both write `artist`. The current
verification prevents damage, but only after preparing a writer and doing
unnecessary work.

Relevant code:

- `src/model/schema.rs`: Actors/Artist mappings.
- `src/tags/plan.rs`: plan construction and atom deduplication.

**Recommended task:** validate planned writes by destination key before the
confirmation dialog. If two staged fields request different values for one
atom, show a direct conflict error. Separately decide the product rule:
separate keys, deliberate mirror behavior, or a derived/read-only Artist row.

### 9. Secure temp creation and durability

**Risk:** low-to-medium. Temp paths are PID-predictable and output tools may
overwrite them. Rename is atomic on one volume, but no file/directory sync is
performed for power-loss durability.

Relevant code:

- `src/tags/write.rs`: `temp_beside`, `TempGuard`, `swap`.

**Recommended task:** reserve a unique sibling temp securely, avoid predictable
names, sync the completed temp before rename, then sync the containing
directory where supported.

## Priority 3 — quality gates and coverage

### 10. Make formatting and strict linting green

`cargo fmt --check` and strict Clippy currently fail. The reported Clippy
issues are mechanical: nested image-result handling, unnecessary `format!`,
`repeat().take()`, and an unnecessary cloned slice.

**Recommended task:** run `cargo fmt`, apply the small Clippy fixes, then make
both checks mandatory in CI.

### 11. Add CI and complete the non-generated media matrix

The fixture suite is valuable but cannot generate the failure-prone cases the
native writer was introduced for:

- iPhone `mebx` timed-metadata tracks;
- GoPro `gpmd` tracks;
- files above 4 GiB / `stco` to `co64` overflow boundary;
- no-space behavior;
- macOS ACL/xattr preservation;
- concurrent external modification.

Keep real fixtures outside git if necessary, but make an opt-in test job or a
documented local fixture harness. Add CI for unit tests, generated fixtures,
formatting, and Clippy.

## Suggested execution order

1. Transactionalize ExifTool writes and verify XMP there.
2. Add pre-write fingerprints.
3. Preserve file metadata and repair mtime restoration.
4. Harden native layout validation and ambiguous-meta refusal.
5. Expand before/after verification manifests.
6. Add plan-time destination-key conflict errors.
7. Fix fmt/Clippy and install CI.
8. Validate against representative `mebx`, `gpmd`, and >4 GiB media before
   considering removal of ffmpeg fallback paths.

## Non-goal recommendation

Do not replace ffmpeg/ExifTool merely to remove subprocesses. The native
writer is appropriately scoped: it owns the specific `mdta` behavior this
project needs while relying on an established box editor for offset arithmetic.
The safety work above is more valuable than moving FFmpeg or Perl into the
process through FFI.
