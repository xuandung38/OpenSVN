# Checkout and update at svn CLI speed (#121)

Branch `perf/checkout-update-121`, PR #142. Date: 2026-10-14. Scope: the checkout/update path
(svn-ra update transport, svn-client update/checkout, svn-wc pristine store, work queue and the
wc.db writes they use). Status is out of scope (separate session).

## Result

Reduced benchmark of the P13 harness (`compare-svn --files 20000 --binaries 1 --binary-mb 16
--update-files 2000`), release build, same machine (4 vCPU Linux VM, Apache 2.4.58 +
mod_dav_svn 1.14.3 over HTTP on 127.0.0.1, svn CLI 1.14.3):

| operation | svn CLI | before (main 9425158) | after (ad6b57f) | after / svn |
|---|---:|---:|---:|---:|
| checkout HEAD | 7.05 s | 201.88 s (svn 6.50 s, x31.0) | **5.45 s** | **0.77** |
| update r1 -> r2 (2000 files + 1 binary) | 1.76 s | 20.97 s (svn 1.98 s, x10.6) | **1.54 s** | **0.88** |

- "after": `--reps 5`, medians, order alternating per repetition (harness default). Per rep,
  checkout svn/engine: 5.93/5.45, 7.05/6.91, 9.34/5.19, 7.11/7.73, 5.67/4.82 s.
- "before": the same harness and machine, `--reps 1` (an engine checkout took 3.4 minutes).
- The same run on a journaled ext4 (loop device, `TMPDIR` there, `--reps 3`): checkout 5.52 s
  (svn) vs 5.03 s (0.91), update 0.90 s vs 0.84 s (0.93).
- Status (not in scope) also dropped from 3.04 s to 1.2 s for a repeat run, as a side effect of
  the statement cache. It is still slower than svn; the status session owns that.

## Profile: where the time went

Measured with `perf` (call graphs, DWARF), `strace -c` and the Apache access log.

- **Network was never the problem.** The access log shows the same requests for both clients: one
  REPORT, then one GET per file (skelta mode, mod_dav_svn's default), keep-alive on, no repeated
  authentication, plain HTTP in the benchmark. 20,001 GETs for each checkout.
- **Per-node durability (≈ 95% of the old checkout).**
  - one wc.db transaction per node with `synchronous=FULL` (several fsyncs each), plus a
    separate transaction for the pristine row;
  - `sync_all` of every pristine and of every working file, plus a directory sync;
  - the work queue spent two more transactions per item (fetch, then delete + fileinfo).
  - The pristine was also copied and hashed a second time after the delta had already hashed it.
- **SQL overhead** once the fsyncs were gone: every statement was parsed again
  (`sqlite3RunParser`, `yy_reduce`, about 8% of CPU), and every read outside a transaction took its
  own shared lock. Update then read the tree one node at a time three times: restore walk,
  reporter, revision bump, about 60k queries for 20k files.
- **Thread ping-pong**: `add_file` and `apply_textdelta` waited for the editor thread's answer,
  so every file cost two round trips between the network task and the editor thread
  (`finish_task_switch` + futex ≈ 18% of the profile). With only 6 downloads in flight, the
  network and the editor waited on each other.
- **Synchronous writeback**: after batching, the remaining `syncfs`/fsync waits were 1.4 s +
  0.5 s of a 6 s checkout. svn never waits for writeback (it runs `synchronous=OFF` and never
  fsyncs working copy files).

## What changed

svn-wc:
- `durable` (new): `sync_filesystem` = one `syncfs` (Linux) instead of per-file fsync;
  `BATCHED_SYNC` is false elsewhere, and writers keep their per-file sync there.
- `PristineStore::place`: moves an already hashed temp file into the store (no copy, no re-hash).
  The row is recorded by the caller in its transaction (`WcTxn::record_pristine`).
  `open_text_with_row` returns the row and the file with one lookup.
- Work queue: self-contained items (`FileInstall` from the pristine, `DirInstall`,
  `SyncFileFlags`, `RecordFileinfo`; `WorkItem::is_self_contained`) are completed together, up to
  4096 per transaction. Any other item is completed on its own before the next one runs, exactly
  like svn's loop, because it may consume what an earlier item needs (an install from a temp file,
  then the removal of that file). Installs skip their own fsync when a `syncfs` barrier precedes
  the completion commit. `run_unsettled` leaves the completion to the caller (the update editor
  folds it into its next batch).
- Prepared statement cache (capacity 128) for the hot statements (`cached_row` / `cached_exec`),
  and per-directory bulk readers `WcDb::base_children` / `current_children`. `install_info`
  runs in one read transaction.

svn-client (update/checkout):
- `update/batch.rs` (new): the editor prepares every node exactly as before, but hands its wc.db
  writes (pristine row, BASE row, ACTUAL props, conflicts, work items) to a batch as a closure. A
  flush stages the batch and starts a `syncfs` on a background thread. The next flush waits for it
  (always already finished in the profiles) and commits the staged closures **in order, in one
  transaction**, together with the completion of the work that ran after the previous commit. It
  then runs the work the batch queued. Flush at 2048 nodes, 64 MiB of new text or 1 s.
  A full drain (synchronous) runs at `close_edit`, on abort, and before a path deleted in the
  pending batch is added again.
- Children of a directory added by the same drive (not a resumed one) skip their wc.db lookups:
  nothing can be recorded below it yet.
- Reporter: one query per directory and one query for all lock tokens. Restore walk: one query
  and one directory listing per directory (a name missing from the listing is confirmed with a
  `stat`, so case-folding filesystems behave as before). Revision bump: one query per directory.
- Progress events are still sent as each node is prepared (needed for cancellation from the
  progress callback, pinned by `cancelled_update_is_resumed_by_the_next_update`).

svn-ra-api / svn-ra:
- `EditorWorker::spawn_pipelined`: the handle assigns directory and file ids itself and the
  worker maps them to the editor's ids, so `open_*` / `add_*` / `apply_textdelta` no longer wait
  for the editor; a failure surfaces on the next call like any queued command. `spawn` keeps the
  old acknowledged behaviour (its tests are unchanged). The HTTP update driver uses the pipelined
  worker.
- Download window: up to 64 files in flight (was 6); still 3 concurrent GETs (4 connections, one
  held by the REPORT, as before).

Shared code touched (kept minimal, all additive or mechanical): `svn-wc/src/db/*.rs` read
helpers switched to the statement cache (`.query_row(` → `cached_row(`), new `base_children` /
`current_children`, `read_repos_info_on` / `repos_lock_on` split out. The status session may
want the same bulk readers.

## Crash safety (adversarial review)

- **Ordering.** Every pristine text and every installed working file is durable before the
  transaction that relies on it commits. Pristines are placed, then the batch's `syncfs` starts,
  then the commit waits for it. Working files of the work that ran after commit N are covered by
  the `syncfs` started after they were written, and their completion commits with batch N+1.
  Without `BATCHED_SYNC` each file is synced on its own as before.
- **Kills.** The committed state is always a prefix of the node sequence, each node all or
  nothing. That equals a kill between two of the old per-node transactions. Uncommitted nodes
  leave their directory `incomplete` (or the old BASE), so the next update fetches them again.
  Placed pristines without a row are orphans the next install overwrites. Work that ran but was
  not completed stays queued and runs again (only self-contained items are deferred: idempotent).
- **Failure mid-drive.** If a staged batch cannot be committed (sync or transaction failure), the
  editor commits nothing prepared after it, also on abort. Otherwise a file could be committed
  below a directory whose row was in the lost batch.
- **No SQLite lock is held between editor calls** (closures are applied only at commit), so a
  slow network never blocks other processes on wc.db.
- **TLS / credentials.** No change to TLS: `CertDecision::is_valid_for` is untouched and the
  default-deny holds. No new logging; the one new `tracing::debug!` (failed commit on abort) prints
  the error only.
- **Compatibility.** wc format 31 rows and work items are byte-identical to before (same writers,
  same skels). There is no protocol change: same REPORT and GETs, valid against HTTPv2 servers
  1.8–1.15.
- **Divergence from svn, kept on purpose:** we still `syncfs` (svn does not sync at all). It costs
  nothing on the critical path now (background), apart from the final drain's barrier (~0.1 s).

## Gates (head ad6b57f)

All on `ad6b57f` (merged with origin/main 05b8955, which includes #138, the #119 fix):

| gate | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 0 warnings |
| `cargo clippy -p opensvn --features real-engine --all-targets --locked -- -D warnings` | 0 warnings |
| `cargo clippy -p svn-wc -p svn-client -p opensvn-crash --features failpoints --all-targets -- -D warnings` | 0 warnings |
| `cargo test --workspace --locked` (oracle: svn 1.15.0 built from the release tarball, `OPENSVN_REQUIRE_SVN=1`) | 1404 passed, 0 failed, 22 ignored |
| `cargo test -p svn-wc --features failpoints --locked` | 242 passed, 0 failed, 1 ignored |
| `cargo test -p opensvn --features opensvn/real-engine --locked` | 487 passed, 0 failed |
| `cargo test -p opensvn --features failpoints --locked` | 493 passed, 0 failed |
| DAV suite `real_engine_dav -- --ignored` (local Apache 2.4.58 + mod_dav_svn 1.14.3 over HTTPS, Basic auth, `mod_authz_svn`, `OPENSVN_COMPAT_LOCAL_URL/_REPOS/_CA/_AUTHZ`) | 20 passed, 0 failed |
| crash suite, random SIGKILL (`CRASH_RANDOM_ITERS=6`) | 35 cases, 0 failed |
| crash suite, named failpoints (`CRASH_FP_ITERS=3`) | 89 cases, 0 failed; every failpoint reached |
| crash suite, random SIGKILL, extra run on 175839d+fixes (`CRASH_RANDOM_ITERS=20`) | 105 cases, 0 failed |

New tests: runner batching (`self_contained_items_complete_together_and_others_alone`,
`run_unsettled_leaves_completion_to_the_caller`,
`carried_items_complete_before_an_item_that_runs_alone`) and `incoming_replacements_vs_svn`
(svn 1.15 oracle).

## Notes, limits

- Measurement noise on this VM is large. The root filesystem is ext4 without a journal, and
  creating files right after an `rm -rf` of a previous working copy is slower
  (`find_inode_bit`/`recently_deleted`), for both clients. The comparisons above alternate the
  order like the harness does; the journaled-ext4 loop run shows the same picture.
- macOS and Windows keep per-file fsync (`BATCHED_SYNC` = false); not measured here.
- More HTTP connections (8, serf's maximum) cut the checkout further in a quick test, but the
  default stays at svn's 4.
- Found while testing, **pre-existing and not touched here**: when the server replaces a file by
  a directory, the engine's working copy differs from svn 1.15's (same result on `main`). It is
  left out of `incoming_replacements_vs_svn`; it needs its own issue.
- Two review findings were fixed in this branch before it left draft. One: a batch that cannot
  be committed now stops every later commit. Two: a re-added path is settled when its deletion
  sits in the staged batch, not only the pending one.
