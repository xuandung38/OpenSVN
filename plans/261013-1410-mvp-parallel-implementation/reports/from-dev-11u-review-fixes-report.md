# 11U review fixes (PR #74, review at b7a788d)

Branch `feat/11u-client-m1`, origin/main merged in first (#60, #77).

## Blocking items
1. **Incoming delete on a locally replaced node.** `delete_node` now picks the reason from the
   working row. `deleted` and `replaced` only remove BASE and record the tree conflict; the local
   rows and the disk stay. Only `edited` keeps BASE as a copy (`keep_as_working`).
   `WcTxn::convert_base_to_copy` now fails if any row already exists at the target op_depth, so it
   can never overwrite a replacement. Oracle tests cover a replaced file, a replaced directory, and
   a delete against a local delete.
2. **Prop edit on a locally deleted file.** A shadowed file (`deleted/edited`) updates BASE
   properties only. Nothing is merged into ACTUAL, no `.prej` is written, and the stat cache is
   cleared like svn. Covered by an oracle test.
3. **Unversioned directory in the way of an incoming add.** Without
   `allow_unversioned_obstructions`, the incoming tree goes into BASE under a `base-deleted` layer
   rooted at the obstruction (nested nodes use the root's op_depth). The disk is not touched, and
   the conflict is recorded as `unversioned/added`. Covered by an oracle test with a nested
   subdirectory and a directory property.
4. **Depth.** `update(.., depth, ..)` now follows `svn update --depth`: the depth is never
   recorded.
   - A narrower depth limits the drive. Nodes outside it keep their revision.
   - Children the server adds outside the depth are recorded as `not-present` (svn issue #3569).
   - `immediates` adds new subdirectories with depth `empty`.
   - `bump_revisions_post_update` takes the depth.
   - Oracle tests cover `empty`, `files` and `immediates`, each followed by a full update to r3.
   - `--set-depth` (crop/expand) is not implemented.

## Non-blocking items
- `bump_revisions` follows svn's `bump_node_revision`: below the root, `not-present` rows are
  removed, and so are `server-excluded` rows at another revision. `skip_when_dir` applies at
  depth `files`.
- A deleted update target keeps a `not-present` row at the new revision (oracle test).
- `status --depth files` skips child directories, versioned and unversioned (oracle test for all
  four depths).
- Merge temp files stay `TempPath` until the transaction commits; on failure they are deleted.
- `$Date$` is expanded in the system time zone through `jiff` (`utc_offset_local`); `jiff` was
  already in the lock file.
- Update failpoint tests: `update.after_merge3_before_txn` and `update.after_txn_before_queue`, each
  followed by a resumed update (`tests/update_failpoints.rs`). A cancellation test resumes a
  cancelled update.
- The fake update planner now visits children reported at their own revision (mixed-revision
  working copies). Before this, server deletes of such children were never sent.

## Public API (svn-wc / svn-client)
- Added: `WcTxn::bump_revisions_post_update`, `WcTxn::mark_base_deleted_at`,
  `translate::utc_offset_local`.
- Changed behaviour:
  - `WcTxn::bump_revisions` drops non-root `not-present` rows.
  - `convert_base_to_copy` refuses an occupied op_depth.
  - `update` depth is no longer sticky.
- No signatures changed.

## Gates (local, svn 1.15.0 from nix on PATH, OPENSVN_REQUIRE_SVN=1)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --exclude opensvn --all-targets -- -D warnings`: pass. The Tauri app
  was excluded; it does not depend on svn-wc or svn-client.
- `cargo clippy -p svn-wc -p svn-client --all-targets` with failpoints: pass
- `cargo test -p svn-client`: 46 passed; with `--features failpoints`: 47 passed
- `cargo test -p svn-wc`: 213 passed; with `--features failpoints`: 215 passed
- `cargo test -p svn-ra-api`: 39 passed; `cargo test -p svn-ra`: 66 passed

## Remaining
- `--set-depth` (sticky depth with cropping) is not implemented.
- Non-UTF-8 names on disk are still skipped by status.
- Files over 64 MiB still become a conflict instead of being merged (documented).
- A locally replaced file with an incoming edit records the tree conflict without updating
  BASE. svn moves BASE on in that case; there is no oracle test for it yet.
