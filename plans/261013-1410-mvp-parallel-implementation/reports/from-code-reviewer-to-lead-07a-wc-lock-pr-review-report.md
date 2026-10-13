# PR #35 review: feat/07a-wc-lock (A2 M8, WC_LOCK obtain/release/steal + ownership recovery)

Date: 2026-10-13. Head `3d14481` (base `feat/07a-wc-nodes` @ `41f2359`). Reviewed from a `git archive` scratch export.

## Verdict: **REQUEST CHANGES**
The obtain/held/steal semantics match `svn_wc__db_wclock_obtain`, and foreign rows block correctly. However, `wc_cleanup_locks` can delete **foreign live locks** through two paths: the documented dead-owner race and the "orphan" sweep. Either one lets OpenSVN write concurrently with a running svn or TortoiseSVN operation. A cheap mitigation that closes the race exists and I verified it against svn 1.15.0 (see below).

## Gates (scratch, includes #33)
- fmt, `clippy -p svn-wc --all-targets -D warnings` (with and without `failpoints`) and `cargo test -p svn-wc` are green (wc_lock 8 tests, incl. the svn-backed ones). `cargo deny` is OK.
- No svn-types change: the existing `LockOwnershipStore` trait is used as is.

## Interop check (foreign lock never deleted except by explicit steal)
| Path | Deletes foreign row? |
|---|---|
| `wc_lock_obtain` | no: foreign rows at or above, or below within `levels`, return WcBusy |
| `wc_lock_release` | only rows in this handle's `owned` map |
| `wc_lock_steal` | yes, explicit by design (OK) |
| `wc_cleanup_locks`, dead-record loop (wc_lock.rs:209) | **yes, in the race below** |
| `wc_cleanup_locks`, orphan sweep (wc_lock.rs:218) | **yes**: any row without a NODES row is deleted, whoever owns it |

## Findings

[IMPORTANT] The dead-owner cleanup can delete svn's or TortoiseSVN's live lock (documented race). This is more likely than the docs suggest. Location: crates/svn-wc/src/db/wc_lock.rs:204-216.
- The sequence is: app crash, then `svn cleanup` (removes all WC_LOCK rows), then a new svn op locks the same dir, then our cleanup deletes that lock.
- Almost every operation, ours and svn's, locks `('', -1)`, so "same directory" is the common case, not a corner case.
- After the delete, OpenSVN can obtain the lock and write while svn is mid-operation.

Assessment of the proposed mitigations:
- *Clean our stale locks as early as possible at startup.* This narrows the window but does not close it: the app can restart hours after the crash, with svn operations in between. Do it anyway, since it is cheap.
- *Remove only if WC_LOCK still matches the recorded (wc_id, local_dir_relpath, locked_levels).* This is a weak discriminator, because svn takes exactly the same `('', -1)` tuple. The rowid does not help either: WC_LOCK is a rowid table without AUTOINCREMENT, so after svn cleanup empties it, svn's new row gets the same rowid (max+1).
- *Remove only if no SQLite lock is held.* This does not work. svn holds a WC_LOCK row for the whole operation but holds SQLite locks only during individual statements, so svn would usually look idle.

**Recommended mitigation (verified):** add a per-lock marker file in `.svn/tmp`.
- At obtain: commit the row first, then create `.svn/tmp/opensvn-wclock-<hash(dir)>-<pid>-<boot_id>` and fsync it. Record it next to the ownership record, or derive the name from it.
- At release or steal: delete the marker.
- In cleanup, auto-delete a dead owner's row **only if its marker still exists**. If the marker is gone, treat the row as foreign: report HeldByOther and require explicit steal.
- Why this is sound: `svn cleanup` is the only svn path that removes another process's WC_LOCK rows (break-locks), and it always empties `.svn/tmp`. I verified on svn 1.15.0 that `svn cleanup` removed `.svn/tmp/opensvn-lock-123` and a subdirectory **and** the WC_LOCK rows. Normal svn operations leave unknown tmp files alone (the marker survived a refused `svn update`).
- Failure modes all fail safe: a crash between row commit and marker means no auto-delete; a user who deletes the marker gets a prompt instead.
- Order matters. If the marker were written *before* the row, a crash between the two would leave a marker with no row. svn could then lock without any cleanup and our marker would vouch for svn's row.
- Add a test: obtain, simulate a crash, run `svn cleanup`, run `svn` under a sleeping hook (as in the existing E155004 test), then `wc_cleanup_locks` must return `[]` and svn's row must remain.

[IMPORTANT] The orphan sweep deletes foreign rows (wc_lock.rs:218-238). It removes every WC_LOCK row whose dir has no NODES row, including rows held by a running svn or TSVN process. This contradicts the module's own rule ("never removed except by an explicit steal") and the lead's interop requirement. svn locks a dir before the node exists, or keeps the row while the node is removed, in some sequences (e.g. operations that add or remove directories under a lock taken on that dir), so this is not provably dead state. Fix: restrict the sweep to rows that have one of our ownership records or markers. Leave other rows alone and report them as HeldByOther. `svn cleanup` is the user's tool for those. Update the test `orphan_lock_rows_for_unversioned_directories_are_dropped` accordingly.

[IMPORTANT] Record-before-row makes the race wider and is not "harmless" (wc_lock.rs:142-154). If the process crashes after `store.record` and before the row commits, a dead record exists with no row. svn can then lock that dir *without* any cleanup, and our next `wc_cleanup_locks` deletes svn's lock. The code comment claims the opposite. The same happens if `forget` fails at line 153: the error is swallowed with `let _`. Fix: insert the row, then record ownership, then write the marker. A crash after the row but before the record leaves a row nobody claims, which is the safe direction (it is foreign until stolen). With the marker gate in place, a stale record without a marker can never delete anything.

[MODERATE] Two threads obtaining the same dir in one process can clobber the winner's record (wc_lock.rs:131-155). Both threads pass the in-memory `owned` check and both call `store.record` (same pid, so the record is replaced rather than rejected). One thread's txn wins. The loser gets WcBusy and calls `forget`, which erases the **winner's** record. If the app then crashes, the winner's row is unclaimed and needs a manual steal. Fix: serialize obtain per handle (hold a dedicated mutex across check, record and txn), or let only the code path that inserted the row call `forget`.

[MODERATE] `wc_lock_release` drops the in-memory entry before the DB delete (wc_lock.rs:160-166). If `delete_row` fails (busy after 10 s), the row and the record stay but the handle no longer considers the lock its own. The lock is then stuck for the process lifetime: the owner is alive, so cleanup skips it, and obtain reports busy. Fix: remove from `owned` only after the txn commits, then call `forget`.

[MODERATE] `wc_cleanup_locks` runs one write txn per dead dir plus one for the sweep, and none of them hold our own lock. Concurrent `wc_lock_obtain` on the same handle can interleave. Low risk, but wrap the whole cleanup in one `with_write`.

## Positive (risk calibration)
- The `locked_levels` coverage maths and `find_covering_lock` match svn's `find_wclock`. Extending one's own lock upward matches svn's `owned_locks` behaviour.
- The tests use the real svn: `svn commit` fails E155004 while our row exists, and the engine sees svn's lock during a sleeping pre-commit hook. These are real interop checks, but they skip silently without svn (CI issue as in #29/#33).

## Plan follow-ups
- A2 M8 is complete after IMPORTANT 1-3 are fixed (marker gate, sweep restricted to our rows, row-then-record ordering) and the race test is added.
- P08 (app backend): run `wc_cleanup_locks` for every registered WC at startup, before any UI-driven operation.

## Unresolved questions
1. Is a marker file in `.svn/tmp` acceptable as a product decision? svn treats tmp as scratch, and I confirmed that svn cleanup removes it. Alternative: store the wc.db file-change-counter at our last commit in the ownership store, but that requires extending `LockOwnershipStore` (a svn-types contract change).
2. Does TortoiseSVN's cleanup with "Break write locks" also clear `.svn/tmp`? It uses the same `svn_wc_cleanup4`, so it should, but I have not verified it on Windows.

Status: DONE_WITH_CONCERNS
Summary: REQUEST CHANGES. Lock semantics are correct, but cleanup can delete live foreign locks (the documented race, the orphan sweep, and record-before-row). A `.svn/tmp` marker gate, verified against svn 1.15.0's cleanup behaviour, closes the race.

---

## Re-review (head `6270216`, base `2be4ea9`, 2026-10-13)

**Verdict: APPROVE** (one non-blocking residual, already tracked: TortoiseSVN "Break write locks" clearing `.svn/tmp` on Windows).

Gates (scratch, `CARGO_BUILD_JOBS=2`): fmt OK; `clippy -p svn-wc --all-targets -D warnings` OK, also with failpoints; `OPENSVN_REQUIRE_SVN=1 cargo test -p svn-wc --features failpoints` green (wc_lock 12/12, including the real-svn tests); cargo deny OK.

| Earlier finding | Status | Evidence |
|---|---|---|
| Dead-owner race deletes a live svn lock | FIXED | Marker `.svn/tmp/opensvn-lock-<nanos>-<pid>-<n>` (skel `(pid boot dir levels)`, `create_new`, fsync). Cleanup removes a row only when the store reports a dead owner **and** a marker for that dir exists, and only `WHERE dir = ? AND locked_levels = marker.levels`, in one txn. A dead record without a marker goes to `needs_user_action`; the row is left alone. The real-svn test `svn_cleanup_after_a_crash_then_a_new_svn_lock_is_never_taken_over` (crash, `svn cleanup`, svn commit held by a sleeping pre-commit hook, our cleanup) asserts `recovered=[]` and HeldByOther, and passes. |
| Orphan sweep deletes foreign rows | FIXED | Sweep removed. Rows of unversioned dirs without our traces are left (`rows_of_unversioned_directories_without_our_traces_are_left_alone`). Only *markers* whose row is gone get swept, never rows. |
| Record-before-row | FIXED | Order is row commit, then record, then marker. A crash after the row leaves an unclaimed (foreign) row; a crash before the marker leaves no marker, so nothing is ever auto-deleted. If `record` fails, the code removes only its own just-inserted row. |
| Racing obtains clobber the record | FIXED | Per-handle `lock_gate` mutex around obtain/release/steal/cleanup. Across handles the row PK decides the winner before any record is touched (`racing_obtains_keep_the_winners_ownership_intact`). |
| Release drops the map entry before the DB delete | FIXED | Map entry removed only after the row delete commits. |
| Cleanup spread over many txns | FIXED | All row removals in one `with_write`. |
| Early recovery | ADDED | `open_and_recover(wcroot, owner)` runs cleanup right after open (writable WCs only). P08 should use it as the only open path. |

Extra empirical check (svn 1.15.0): an unknown `.svn/tmp/opensvn-lock-x` survived `commit` ×2, `update -r1`, `update`, `mv` and `revert -R`, and was removed only by `svn cleanup` (including `--vacuum-pristines`). This confirms the design assumption.

Non-blocking notes:
- Cleanup matches the marker by `dir` only, not by the dead owner's pid/boot (the store API returns dirs only). Safe today because a dir has at most one row and one live record. If the store ever returns (dir, pid, boot), compare them too.
- `needs_user_action` forgets the record, so the hint is reported once. P08/P12 must surface it on that first report.
- `open_and_recover` fails the whole open when the ownership store errors. Consider logging and continuing, since recovery is optional and a store failure should not block read-only use.

Status: DONE. Merge after #33's one-line fix (stacked).
