# 11c (PR #76): merge of #74 fixes (7676db1) and main

## Done
- Merged `origin/feat/11u-client-m1` (head 7676db1) and `origin/main` (d48cb45) into
  `feat/11c-client-m1` as merge commits.
  - `crates/svn-wc/src/db/txn.rs`: kept both sides (imports `LockInfo`, `Presence`, `CommitInfo`;
    `commit_node`, `lock_add`, `lock_remove` next to `base_children` and `bump_revisions_post_update`).
  - `Cargo.lock`: svn-wc deps now list both `ignore` (main) and `jiff` (#74).
  - `bindings.rs` merged cleanly. It uses `utc_offset_local`.
- `crates/svn-client/src/commit/file_commit.rs`: `$Date$` is now expanded with
  `utc_offset_local`, as svn does and as the update path already does.
- New test `crates/svn-client/tests/commit_keywords_local_time.rs`. It re-runs itself in a
  child process with `TZ=XST-7`, then commits a file with `Date Rev Author Id` keywords. It checks
  that the text is unchanged after an update and is byte-identical after the update restores the
  deleted file through `file-install`. With `utc_offset_zero` the test fails, so it detects the
  bug.
- #74 behaviour changes checked against this branch: commit code does not call
  `bump_revisions` or `convert_base_to_copy`, and it does not depend on a sticky update depth.

## Gates (svn 1.15.0, OPENSVN_REQUIRE_SVN=1)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass (full workspace)
- `cargo clippy -p svn-wc -p svn-client --all-targets --features svn-wc/failpoints,svn-client/failpoints -- -D warnings`: pass
- `cargo test -p svn-wc -p svn-ra-api -p svn-client --all-features`: 367 passed, 0 failed, 1 ignored (35 binaries)

## Remains
- The fake RA still sends no `svn:entry:committed-*` in update drives. This belongs to #74.
