# 11C svn-client commit and local operations: ready for review

PR #76, branch `feat/11c-client-m1`. This PR depends on #74 (`feat/11u-client-m1`), which is merged into this branch with a merge commit. Merge #74 first. This PR's diff then shrinks to the 11C work.

## What changed in this round

- **Uses the svn-wc API.** `crates/svn-client/src/local/wc_adapters.rs` is deleted. svn-client no longer depends on `rusqlite`, and that includes its tests. The replacements are:
  - pristine access: `WcDb::pristine_store`
  - queue runs: `crate::wc::run_work_queue`, which calls `run_work_queue_with` with `DbWorkingCopyOps`
  - repository location: `read_repos_info`
  - batched writes: `WcDb::with_txn`
- **New svn-wc writers.** `WcTxn::commit_node`, `WcTxn::lock_add`, `WcTxn::lock_remove` (svn's `lock_add` / `lock_remove`) and `WcTxn::set_props`. There is also a public `NodeInstallInfo::keyword_context`.
- **file-commit handler.** `commit::file_commit::file_commit_fixup` follows svn's `process_commit_file_install`. It:
  - translates the working file from normal form again, so keywords get the new revision and line endings follow `svn:eol-style`;
  - syncs the read-only and executable bits;
  - records the stat cache only when the file equals the committed text.

  It is installed in every queue run of the crate, which includes the update and checkout runs from #74.
- **Crash-safe post-processing.**
  - The journal records the commit. Then one transaction writes the node rows, the released lock rows and the `file-commit` items. The queue runs after that.
  - Like `svn_wc__db_global_commit`, finalize no longer writes a stat cache. The work item writes it.
  - New failpoint: `commit.after_finalize_before_queue`.
- **Fixes found by the oracle test.**
  - `propset` of `svn:keywords` or `svn:eol-style` now clears the stat cache (svn's `clear_recorded_info`).
  - `op_copy` now copies `translated_size` and `last_mod_time` (`STMT_INSERT_WORKING_NODE_COPY_FROM`).
- **Copies in the fake.** The fake commit editor now accepts copies (`add_file` / `add_directory` with `copy_from`).

## Tests against real svn

The svn on PATH is 1.14.3. That is too old for the oracle, which skips it even with `OPENSVN_REQUIRE_SVN=1`, so the runs used the nix build:

`OPENSVN_REQUIRE_SVN=1 OPENSVN_SVN_BIN=/nix/store/ljynfkwbrq7l57mgwif58kivljp8zmwb-subversion-1.15.0/bin/svn OPENSVN_SVNADMIN_BIN=.../svnadmin`

No test skipped.

- **`tests/commit_vs_svn.rs`.** svn 1.15 commits a set of local changes to a `file://` repository. This client commits the same changes to the fake session that replays that repository. The test then checks:
  - the two working copies match: files, exec and read-only bits, NODES and ACTUAL rows, `svn status`;
  - the two repositories match: repository-normal texts and properties of every file, and fresh checkouts.

  Coverage: keywords, a CRLF file under `eol-style=native` (rewritten to LF), executable, needs-lock, directory properties, adds, deletes, copy, move, and a second commit on top.
- **`tests/commit_failpoints.rs`.** A crash after the post-commit transaction leaves a `file-commit` item. Each of these finishes it:
  - this client's cleanup;
  - **svn 1.15's own `svn cleanup`**, which shows the queued item uses svn's format;
  - the next write operation.

  A file edited during that window stays modified.

## Gates (local)

- `cargo fmt --all --check`: ok
- `cargo clippy -p svn-wc -p svn-ra-api -p svn-client --all-targets --all-features -- -D warnings`: ok. **This is scoped, not the whole workspace.** The disk was full: the shared volume reached 0 bytes free during the run. The Tauri app only uses the `svn-ra-api` fake. Its public API did not change, but the app was not rebuilt.
- `cargo test -p svn-wc -p svn-ra-api --all-features`: 254 passed, 0 failed
- `cargo test -p svn-client --all-features` (failpoints on): 87 passed, 0 failed
- `cargo deny`: not run, because it is not installed.

## Still open

- The fake update drive sends no `svn:entry:committed-rev`. A checkout from the fake therefore expands `$Rev$` as `-1`. This belongs to #74 and the fake, not to commit. The oracle test compares keyword files in repository-normal form for this reason.
- Unchanged from before:
  - resolve: `mine-conflict` / `theirs-conflict` choices
  - cleanup: removing unversioned and ignored files
  - lock / unlock commands
- Workspace-wide clippy and `cargo deny` need a run on a machine with disk space.
