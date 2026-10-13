# WP1 — commit journal safety (#85)

Branch `feat/wp1-commit-journal-safety`, closes #85. Plan: `real-engine-wiring-plan.md` WP1.

## What changed

**svn-ra-api** (additive only; WP0 overlap kept small)
- `CommitObserver::on_merge_sending(txn)` (default no-op): called right before MERGE; an error vetoes the MERGE (txn deleted, nothing merged).
- `RaSession::abort_txn_checked(txn, cancel) -> TxnAbort { Deleted, Gone }`; default returns `ServerUnsupported` so "unknown" is never mistaken for "deleted".
- Fake: `close_edit` calls `on_merge_sending`; `FakeRaSession::fail_next_merge(FakeMergeFault::{LostBeforeMerge, LostAfterMerge})` → `CommitOutcomeUnknown` with txn present / gone; `abort_txn_checked`; adds into a dir deleted on the server → `OutOfDate` (svn_code 160013), name added on the server too → `OutOfDate` (160020), like svn's `fixup_commit_error`.

**svn-ra**
- HTTP editor calls `on_merge_sending` before MERGE (veto → txn deleted).
- `ops::commit::abort_txn_checked`: DELETE plain then virtual name; any 2xx → Deleted, a 404 (no 2xx) → Gone, only 400s / other errors → error.
- #75 nit: doc comments on `refusal_is_definite`, `CommitFailure::NotMerged`, `HttpCommitEditor`, `CommitEditor::close_edit` no longer claim a refusal proves nothing merged (mod_dav_svn can answer 500/404 after the revision is written on storage faults).

**svn-client**
- Journal format v2 (`merge_sent`, `start_youngest`, `message`, `author`); v1 still read. App journal rules ported: unknown version untouched (and blocks commits of its WC when it names it), corrupt → quarantined via no-replace move (`.corrupt`, `.corrupt.N`), unreadable kept, never deleted.
- `commit()`: fetches youngest before creating the txn; explicit `CommitOutcomeUnknown` branch keeps journal + txn, never aborts; a MERGE refusal after `merge_sent` is reconciled before the journal is dropped; double failure (merged write fails + finalize fails) retries the merged write and otherwise recovery finds the revision in the log.
- `finalize_pending_for_wc` refuses (`WcNeedsCleanup`) while a journal of the WC has an unknown outcome.
- Recovery: merge_sent & no rev → `abort_txn_checked`: Deleted → drop; Gone → HEAD ≤ start → drop; else log `start+1..HEAD` matched by author (if known) + message + changed-path set + MD5 of uploaded texts; exactly one → finalize; none/several → keep, `needs_user`.
- New `recover_wc_commits(state_dir, &WcDb, Option<&dyn RaSession>, cancel) -> WcCommitRecovery { finalized, aborted, needs_user, failed }`; `RecoveryReport` gained `needs_user`. `CommitOptions.author` (optional) is journaled for matching.
- Harvest: replaced directories send the lock tokens of every locked node below them (verified against svn 1.15: `svn rm d; svn mkdir d; svn ci` with `d/f` locked commits and releases the lock).
- `map_commit_error` also maps 160013/160020/175005/175007 to `OutOfDate`.

## Tests added
- commit_failpoints (failpoints): crash after MERGE sent (found in log, finalized; next commit refused meanwhile); CommitOutcomeUnknown with txn present (aborted; offline recovery keeps it) and gone (finalized); gone without match → needs_user, journal untouched; double failure (#85 item 5) both with and without the retried write; failure between marker and MERGE; MERGE refused out of date with a look-alike concurrent commit (journal dropped after log check).
- commit_fake (svn 1.15 oracle): replaced dir with locks below vs svn; adds colliding with server changes vs svn (E155011); delete of a conflicted node refused vs svn; recovery keeps unknown-version / quarantines corrupt journals.
- journal unit tests (v1 compat, unknown version, quarantine no-replace, unreadable kept); svn-ra-api fake tests; svn-ra HTTPS tests for `abort_txn_checked` statuses and the MERGE announcement/veto.

## Gates (merged with origin/main 8c04160; svn 1.15.0 oracle, OPENSVN_REQUIRE_SVN=1)
- `cargo fmt --all --check`: pass
- `cargo clippy --workspace --all-targets -- -D warnings`: pass
- `cargo clippy -p svn-wc -p svn-client --all-targets --all-features -- -D warnings`: pass
- `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings`: pass
- `cargo test --workspace --locked`: 1224 passed, 0 failed, 20 ignored
- `cargo test -p svn-wc -p svn-client --all-features`: 369 passed, 0 failed, 1 ignored
- `cargo test -p opensvn --features opensvn/real-engine`: 317 passed, 0 failed (one app test updated: remote mkdir of an existing path now reports `OutOfDate`, the fake's new svn-like mapping)

## Remaining
- WP5b/WP6 wire `recover_wc_commits` into the app and delete `engine/real/journal.rs` (not in WP1 scope).
- Author matching is used only when the caller passes `CommitOptions.author` (the app knows the account user name; RaSession has no accessor).
