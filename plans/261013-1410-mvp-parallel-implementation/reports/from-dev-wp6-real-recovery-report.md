# WP6 real engine recovery: dev report

Branch `feat/wp6-real-recovery` (from origin/main 6fdce95, merged with origin/main c482176 / WP3 #93).

## Done
- `engine/real/recovery.rs`: `UnwiredRecovery` replaced by `ClientRecovery`. `RecoveryHooks` keeps only
  `run_work_queue(&Arc<WcDb>, ctx)`, which runs `svn_client::wc::run_work_queue` inside
  `svn_client::local::lock::with_wc_lock`.
- Commit recovery on open: `svn_client::commit::recover_wc_commits(<state_dir>/commit-journals, db, session, cancel)`.
  The session comes from `connect_with(repo_root, ConnectOptions { interactive: false, anchor: Root })`.
  It is opened only when a journal of this working copy has no revision recorded (or cannot be read).
  If the connection fails, recovery runs with `None`: merged commits are still finalized, and the rest is reported.
  - `finalized` adds `""` to `recovered`.
  - `needs_user` or `failed` adds `""` to `needs_user_action`.
  - `aborted` is only logged.
  - Nothing runs while a foreign lock or unfinished work is left. The working copy is reported instead.
- `RealEngine::commit_journal_dir()` (pub) gives WP5b the same directory.
- `engine/real/journal.rs` and `RecoveryHooks::finalize_commit` are deleted. Legacy migration: if
  `<state_dir>/commit-journal/<sha1(canonical root)>.json` exists for the working copy:
  - It is moved to `.legacy`, or `.legacy.N` if that name is taken. The move never deletes the file and never replaces an existing copy.
  - The working copy is reported as needing the user, once.
  - If the file cannot be moved, it stays in place and is reported on every open.
- svn-client changes from the #92 review:
  - In `finalize_pending_for_wc`, an `Unreadable` journal blocks commits (`WcNeedsCleanup`). So does an unknown-version journal that names no working copy.
  - `JournalData::outcome_unknown()` now also covers a v1 journal that has a `txn` and no revision. It goes through `abort_txn_checked` plus the log search and is never aborted blindly. With no `start_youngest`, a v1 journal whose transaction is gone ends up as needs_user.
  - The log search is limited to `RECOVERY_LOG_SEARCH_LIMIT = 1000` revisions. Past that, `Reconciled::NotSearched` keeps the journal and reports needs_user. In the commit path it is handled like `Ambiguous`.
  - In svn-ra `checked_abort_tells_deleted_from_gone`, added rows: 403/404, 404/403, 403/403 and 404/500.
- App feature `failpoints = ["real-engine", "svn-client/failpoints"]`. `real-engine` is included so that `--features failpoints` builds the engine. `fail` is a dev-dependency. A CI step was added.

## Tests
- `tests/conformance_recovery_tests.rs` runs `conformance::open_recovers_dead_locks_and_reports_foreign_ones` against `RealEngine`. Checkout is done by `svn_client::checkout` through a delegating facade until WP4 wires `checkout`. Passes.
- `recovery_tests.rs` covers:
  - never connecting without journals, and an unattended connect (`[false]`)
  - leaked transaction deleted
  - unknown outcome with the transaction present (dropped) and with the transaction gone and no log match (kept, needs_user)
  - Network abort failure and offline (kept, needs_user)
  - corrupt, unknown-format and unreadable journals
  - foreign lock and stuck work queue block recovery
  - legacy journal handling, including a full set of copies
  - LostAfterMerge finalized on open, LostBeforeMerge dropped, a merged commit that waits for a foreign lock
- `tests/real_recovery_failpoints.rs` (only with `--features failpoints`, as its own binary) simulates a kill with failpoints plus a planted dead-process lock:
  - kill after MERGE sent: finalized on the next open over an unattended session
  - kill after the revision was recorded: finalized while offline
  - MERGE sent with an unknown outcome: needs_user, the journal is kept, and the next commit is refused
  - kill mid-update (work queue left behind): the open runs the queue, the file is installed, and the next update completes
- Mutation check: passing `None` as the recovery session makes the post-MERGE kill test fail.

## Gates (svn 1.15.0, OPENSVN_REQUIRE_SVN=1, run on the merged tree)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p svn-client -p svn-wc --all-features --all-targets -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass. With `--features failpoints`: pass
- `cargo test --workspace --locked`: 1242 passed, 0 failed, 21 ignored
- `cargo test -p svn-client --all-features`: 146 passed, 0 failed
- `cargo test -p svn-ra --test commit_cleanup`: 5 passed
- `cargo test -p opensvn --features opensvn/real-engine`: 339 passed, 0 failed
- `cargo test -p opensvn --features failpoints`: 343 passed, 0 failed (includes the 4 crash tests)

## Notes / remaining
- Journals are keyed by `WcDb::wcroot()`. WP5b must open the working copy the same way (`open_containing` or `find_wcroot`) so that commit and recovery agree on the root spelling.
- `ConnectOptions { interactive: false }` relies on WP2's `HttpConnector` to return an `.unattended()` session. The test connectors only record the flag.
- A `failed` entry can be another working copy's corrupt or unreadable journal, since its working copy cannot be known. It is reported for every working copy that is opened. Corrupt files are reported only once, because they are set aside.
- The new merge commit message lacks the Co-Authored-By trailer (the default merge message; per the rules it was not amended).
