# Crash safety fixes: #119, #120, #137

PR #138 (`fix/crash-safety-119-120`).

## Result

Crash suite at `CRASH_SEED=20261014` with default knobs (`CRASH_RANDOM_ITERS=6`, `CRASH_FP_ITERS=3`, `CRASH_FILES=200`):

| suite | cases | failed |
|---|---:|---:|
| random SIGKILL | 35 | 0 |
| named failpoints | 91 | 0 |

The failpoint run has 2 more cases than the 89 the #117 reviewer reported, because it now covers two new failpoints (`wc.create.before_schema` and `wc.create.before_rename`) with 1 kill each.

One warning remains in the failpoint run, and it is expected: "lock row without ownership marker after kill; needed `svn cleanup`". The crash hit between committing a WC_LOCK row and writing its marker. By design (`wc_lock.rs`), recovery leaves such a row to the user. No `.svn/tmp not empty after recovery` warning remains.

## #119: an interrupted update could not be resumed

**Root cause.** `svn-client/src/update/reporter.rs`, `Ctx::report_dir` reports a child only when it differs from its parent (revision, switched, depth, lock). An `incomplete` directory is reported `start_empty`, so the server assumes it has no children. Children at the directory's own revision were therefore never reported. Their deeper entries were reported below a parent the server did not know, and mod_dav_svn rejected the report (`Working copy path 'bin' does not exist`, 404). The update then failed with a cut-off REPORT response on every retry.

**Fix.** The reporter now does what libsvn_wc does with `report_everything`: under an `incomplete` directory, it reports every child, both files and directories.

**Test.** `crates/svn-client/tests/update_report_incomplete.rs` uses the row layout from the issue. It checks:
- the report lists every child of each incomplete directory;
- every reported path hangs below a parent the server knows;
- the update resumes and ends at r2.

The test fails without the fix (`bin` is not reported).

## #120: `.svn/tmp` leftovers and `wq-*` files in user folders

1. **`wq-*` files in user folders.** `file-translate` in the work queue runner created its temp file next to the destination. A kill before the rename left `d0/wq-XXXX` in the user's folder. The temp file now goes in `.svn/tmp`, like `file-install` and like libsvn_wc.
2. **Stale files in `.svn/tmp`.** The new `WcDb::remove_stale_tmp_files` removes them. It runs from `cleanup`, from the app's recovery on open (best effort: a failure is logged and never blocks the open), and from the crash suite's recovery. Safety rules:
   - It runs only while the handle holds the write lock on the whole working copy (`'' , -1`) and the work queue is empty. No live engine operation can still be writing a temp file then, and no queued item can name one.
   - It removes only regular files whose name has a known engine prefix (`wq-`, `update-`, `merge-`, `pristine-`, `marker-`, `external-`) followed by a suffix, plus `wc.db.creating*`.
   - It never removes lock markers (`opensvn-lock-*`), svn's own temp files, directories or symlinks. It never follows a symlinked `.svn` or `tmp`.

   Test: `crates/svn-wc/tests/wc_lock.rs::stale_tmp_files_of_the_engine_go_and_nothing_else`.

Remaining case: `special::install_from_reader` (a `svn:special` file that is not a link) and the cross-device `copy_then_remove` still create their temp file next to the target. Both are rare and out of scope here.

## #137 (new): 0-byte `wc.db` after a checkout killed early

**Root cause.** `WcDb::create` opened `.svn/wc.db` with `SQLITE_OPEN_CREATE` under its final name. A kill before the schema commit left an empty `wc.db`. From then on, checkout refused to run ("already holds a working copy database"), and open reported "format 0 too old, run svn upgrade". svn 1.14 failed too (E200030).

**Fix** (new module `crates/svn-wc/src/db/create.rs`):
- **Atomic creation.** The database is built as `.svn/tmp/wc.db.creating`, closed, and renamed to `.svn/wc.db` (followed by a directory fsync). After that, `format` and `entries` are written. The empty TRUNCATE journal of the temp file is removed. The resulting file is byte-identical in schema to before: format 31, readable by svn 1.8–1.15, and `.svn/tmp` is left empty.
- **Ownership.** Whoever holds the OS file lock on `.svn/tmp/wc.db.creating.lock` owns `.svn`:
  - A creator never deletes anything before it holds the lock.
  - The lock holder checks that the locked file is still the one at that path (dev and inode on Unix). This protects against a lock taken on a file that a finishing creator has just deleted.
  - On a filesystem without file locks, creation still works, without the concurrency protection.
- **Restart.** A *skeleton* consists of:
  - `tmp`, holding only the lock file, the temp database and its journal;
  - optionally an empty `pristine`;
  - for earlier versions, an empty or schema-less `.svn/wc.db` and its journal.

  The skeleton must carry evidence of a creation: the lock file, or the empty in-place database. A checkout run again into the directory takes such a skeleton over in place, once it holds the lock and has re-inspected the skeleton under it:
  - it deletes only the listed files, one by one;
  - it never deletes a directory;
  - it never touches anything else.

  An empty `.svn`, or `.svn/tmp` alone, is not treated as a skeleton. Neither is anything with extra files, a non-empty `pristine`, or a `wc.db` that has a schema or that SQLite cannot read. All of those are rejected untouched, with a clear error.
- **Open.** `WcDb::open` reports an empty or uninitialized `wc.db` as `NotAWorkingCopy`, with the message "holds the remains of an interrupted checkout …; run the checkout again to restart it", instead of "format 0, run svn upgrade". An empty `wc.db` is considered uninitialized only when no non-empty journal sits next to it. Otherwise SQLite rolls the journal back first.
- **Crash coverage.** The new failpoints `wc.create.before_schema` and `wc.create.before_rename` are added to the crash suite's `FAILPOINTS`.

**Tests:**
- `crates/svn-wc/src/db/create/tests.rs` (9 tests): final layout, takeover of a killed skeleton, the empty database of earlier versions, foreign areas left untouched (6 variants), areas without evidence, a live creator, symlinks, and concurrent takeovers with exactly one winner.
- `crates/svn-client/tests/checkout_restart.rs`: update reports an interrupted checkout, checkout restarts and keeps the user's unversioned file, and a second checkout into the finished working copy is refused.
- The existing `concurrent_creators_have_one_winner…` test caught a race in the first version (removing and recreating `.svn`). The takeover-in-place design fixes it.

## Gates (final head)

| gate | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings |
| `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings` | 0 warnings |
| clippy `--features failpoints` on `svn-wc`, `svn-client`, `opensvn`, `opensvn-crash` (all targets, `-D warnings`) | 0 warnings |
| `cargo test --workspace --locked` | 1378 passed, 0 failed, 22 ignored |
| `cargo test -p opensvn --features opensvn/real-engine` | 474 passed, 0 failed |
| `cargo test -p svn-wc --features failpoints` | 239 passed, 0 failed, 1 ignored |
| `cargo test -p svn-client --features failpoints` | 209 passed, 0 failed |
| `cargo test -p opensvn --features failpoints` | 480 passed, 0 failed |
| crash suite, random SIGKILL (seed 20261014) | 35 cases, 0 failed |
| crash suite, failpoints (seed 20261014) | 91 cases, 0 failed |

## Adversarial review notes

- **Data loss.** Only verified engine files are deleted. Cleanup deletes nothing while a work item is pending or without the whole-tree lock. A takeover re-inspects under the lock. A failed creation removes `.svn` only while the creator holds the lock it took over or created.
- **Path escape.** `.svn`, `tmp` and `pristine` must be real directories (checked with `symlink_metadata`). Symlinked entries are never followed or removed.
- **svn compatibility.** Same schema, same rows, same `format`/`entries` markers. The temp names live in `.svn/tmp`, which svn's own cleanup empties.
