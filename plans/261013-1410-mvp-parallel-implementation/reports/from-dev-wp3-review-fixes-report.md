# WP3 review fixes (PR #93)

Branch `feat/wp3-real-status-diff`. Merged origin/main (793c6f6, WP0 #91) first; clean merge.

## Blocking items
1. **too_large without hunks.** Contract decision (doc 03 + `FileDiff` doc comments): `hunks`
   is optional with `tooLarge`; an engine may send hunks or nothing (the real engine never reads
   a side past 5 MB, so it sends no hunks and keeps the property rows). `diff-view.tsx` shows
   the large-file banner (Load full / external tool) whenever `tooLarge` is set and there are no
   text sides, with a separate hint when there are no hunks. New FE test
   `features/wc/diff/diff-view.test.tsx` (banner without hunks, with property rows, with hunks,
   forced load). Engine tests: `a_too_large_diff_keeps_its_property_rows`, repo side over the cap.
2. **Unversioned files.** `read_local` falls back to `read_unversioned` when there is no present
   node: one-sided addition of the bytes on disk, same `CAT_LIMIT`/`too_large` and binary
   sniffing. Path safety: `RelPath` (no `..`/absolute), no `.svn` component, no symlinked parent
   folder (`WcInvalidOperation`), a symlink is shown as `link <target>` and never followed;
   `against=Some(rev)` on an unversioned file is `PathNotFound`. Tests: equality with FakeEngine,
   conflict marker files after an update conflict, binary, over/at the cap, folder, escaping paths.

## Alignment
- `opensvn_ipc::status_map::map_lock` never fills `token` (both engines); `wc_map` no longer
  strips it. FakeEngine strips its stored token in `status_of`. Field kept (documented as always
  `None` over IPC); removing it is a binding change left for later.
- FakeEngine: deleted/missing file size `None`; the root is listed when its properties changed;
  `ignore_add` with `svn:ignore` scope sets the folder's `svn:ignore` property (local scope keeps
  the per-WC list, cleanup uses the same ignore check); `diff_wc` on a folder returns its
  property rows (with a revision: `WcInvalidOperation` like real), file diffs carry property rows.
- FE: path `""` is shown as "<WC name> (working copy root)" in the Changes list and diff header.

## Non-blocking
- Working file read is streamed through the translator with `take(limit + 1)`; no stat-size
  trust.
- `against=rev`: binary decision uses the working MIME type, then that revision's properties
  (BASE when they are unknown).
- Local-scope ignores: #94 not merged yet; TODO(#94) left in `do_status`.
- Not addressed: `remote_rev` meaning difference, root-anchored `repo_side` (WP2).

## Gates (local, CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test -p opensvn -p opensvn-ipc`: opensvn 249 passed; opensvn-ipc 4 + 6 + 17 + 3 passed
- `cargo test -p opensvn --features opensvn/real-engine`: 338 passed, 0 failed
- `pnpm gen:ipc`: bindings regenerated (doc comments only); drift test passes
- FE: install, lint, typecheck, lint:safety pass; test 29 files / 204 tests pass; build +
  check-dist ok
