# Commit: svndiff deltas against BASE and a faster commit path (#132, commit part of #121)

Branch `perf/commit-delta-132`, PR #140. Scope: the commit path (svn-ra commit editor, svn-client
commit). Shared changes in svn-wc and svn-ra are listed under "Shared changes" below.

## Result

Release build, 4 vCPU, Apache 2.4.58 + mod_dav_svn 1.14.3 over HTTP on 127.0.0.1, svn CLI 1.14.3.
Benchmark: `compare-svn-commit` (new, `benches/`). Each client commits to its own repository and
working copy. The bytes are what the client sent to the server, counted by a proxy.

| operation | svn CLI | engine before | engine after | svn bytes | before bytes | after bytes |
|---|---:|---:|---:|---:|---:|---:|
| add 10,000 files (the #121 commit case) | 25.53 s | 113.53 s | 26.80 s | 15,795,089 | 11,176,702 | 16,019,807 |
| modify 1,000 of them | 3.24 s | 12.64 s | 3.25 s | 541,183 | 1,218,138 | 561,562 |
| 1 byte changed in a 40 MiB file (#132) | 0.75 s | 1.78 s | 0.71 s | 10,465 | 41,952,966 | 10,864 |

"after" is the median of 3 alternating repetitions on the final head. "before" is one run on
`origin/main` 9425158.

The add case depends on server time. Its `MERGE` alone takes between 4.7 s and 15.3 s from one
run to the next on this machine, for either client. An earlier 3-repetition run gave
24.65 s (engine) vs 27.50 s (svn). Subtracting the server time (sum of `%D` in Apache's access
log) gives the clients' own work for the same commit:

| add 10,000 files | run 1 | run 2 |
|---|---:|---:|
| svn CLI, total − server time | 4.44 s | 4.02 s |
| engine, total − server time | 3.95 s | 4.09 s |

Where the engine's time went before (add 10,000 files, 113 s): harvest 1.8 s, prepare 2.8 s,
uploads 14.5 s (one PUT at a time, each waiting for the previous one), MERGE 11.6 s, finalize
29 s, work queue 43 s. The last two were per-file fsyncs: about 18 per committed file.

## What changed

### Delta upload (svn-delta, svn-ra commit)
- `svn_delta::delta_window` implements svn's xdelta:
  - an index of 64-byte blocks of the source view, by a rolling Adler-32;
  - each hit is checked byte for byte, then extended backwards over the pending literals and
    forwards;
  - only `CopyFromSource` and `NewData` ops are emitted.
  - Windows follow `svn_txdelta`: each 100 KiB target window comes with the next 100 KiB of the
    base.
- The commit `PUT` of a modified file is a delta against its pristine. It carries
  `X-SVN-Base-Fulltext-MD5`, plus `X-SVN-Result-Fulltext-MD5` where the server announces
  `put-result-checksum`.
- **Stale or damaged base**: the delta is kept only when the base file read on disk has exactly
  the size and MD5 the working copy recorded for it. Otherwise the full text is sent. The server
  applies the delta to its own base after checking the base MD5 header against it, so a delta
  can never be applied to the wrong text. For bases over 1 MiB the check runs on a second thread
  while the delta is computed.
- **svndiff version, as libsvn_ra_serf picks it**:
  - svndiff 2 (LZ4) when the session's opening `OPTIONS` took under 5 ms (connect included), as
    on a LAN;
  - otherwise svndiff 1 (zlib level 5, `SVN_DELTA_COMPRESSION_LEVEL_DEFAULT`);
  - svndiff 0 when the server announces neither.
  - Captured on the wire: svn 1.14 also sends svndiff 2 to this local server.
- Encoded streams up to 1 MiB stay in memory. Larger ones go to a spill file, which keeps the
  body replayable after a 401 or a reset.

### Upload pipeline (svn-ra commit editor)
- `close_file` returns once the file's text has been read and encoded. The `PUT` and the file's
  `PROPPATCH` then finish in the background, while the drive goes on and the next file is
  encoded. `close_edit` waits for all of them before the `MERGE`, so any failure still stops the
  commit before anything is merged.
- Every request that writes into the transaction takes one turn at a time: PUT, PROPPATCH,
  MKCOL, COPY and DELETE. mod_dav_svn on FSFS fails ("next-id file corrupt") when two writes go
  into one transaction at once, and libsvn_ra_serf never sends them in parallel. The old editor
  could already send up to 4 concurrent PUTs when a driver called `apply_text` for several files
  before closing them; svn-client's drive never did.
- **Contract change**, documented on `CommitEditor::close_file`:
  - the sources may still be dropped after `close_file`;
  - local failures (checksum mismatch, unreadable source) are still reported by `close_file`;
  - a refusal by the server (locked, out of date, authz) now surfaces from `close_edit` instead,
    always before the MERGE, and the transaction is deleted as before.
  - `compat_write` and `ops_commit` follow this contract.
- Journal and recovery semantics are unchanged: `on_txn_created` before content,
  `on_merge_sending` (`merge_sent` marker) right before the MERGE, `CommitOutcomeUnknown` and its
  kept transaction.

### Post-commit (svn-client commit, svn-wc additions)
- **prepare**: the journal copies of the committed texts are written, then synced several at a
  time (8 threads) with one directory sync, instead of one fsync per file in sequence. The texts
  are still durable before the transaction is created.
- **finalize**:
  - the journal copies become the pristines by rename (`PristineBatch::adopt_trusted`; copied
    when the state directory is on another file system);
  - pristine directories are synced once each;
  - the pristine rows join the single finalize transaction (with the node rows, lock rows and
    `file-commit` items).
  - Recovery (`finalize`) verifies every copy it adopts (`adopt`). When a crash came after the
    rename but before the transaction, recovery finds the text already in the store, verifies it
    there and records it (new failpoint test).
- **Work queue**: the `file-commit` items are finished 1,000 per transaction. The fixups run
  first, then one transaction stores their stat caches and deletes the items, which is the
  runner's guarantee per item. Any other item goes through the regular runner.
- **Harvest**:
  - one read transaction;
  - the conflict, changelist, property and lock queries are skipped for nodes without an
    `ACTUAL_NODE` or lock row (prefetched once);
  - each node is read once;
  - text comparisons hash SHA-1 only.
- svn-wc statements that run once per node in these steps are cached on the connection:
  - node row reads (`query_one`);
  - `ACTUAL_NODE` reads and writes;
  - work queue inserts;
  - `commit_node` with `detach_moves` and `layer_nodes`.
  - Statement preparation was most of the CPU time of harvest and finalize.

## Shared changes (outside the commit path)
- svn-wc, all additive:
  - `PristineBatch` (`PristineStore::batch`, `stage`, `adopt`, `adopt_trusted`, `sync_dirs`);
    `install` was split into helpers it shares with it, with unchanged behaviour;
  - `WcTxn::record_pristine` and `WcTxn::complete_work_items`;
  - `WcDb::read_txn` and `WcDb::actual_rows_below`.
- svn-wc statement caching (`prepare_cached`), with no SQL change:
  - `nodes::query_one`, so every `read_node`/`read_base`/`read_current`: this also speeds up
    status and update;
  - `actual::{ensure_row, prune, read_props, write_props, read_changelist, write_changelist}`;
  - `ops::queue_work`;
  - `ops::{commit_node, detach_moves, layer_nodes}`.
- svn-client `local/content.rs` and `local/state.rs`:
  - `detranslate_sha1`;
  - `text_modified` (used by status too) hashes SHA-1 only;
  - `detranslate_to_file` no longer syncs (its only caller, commit, syncs in a batch).
- svn-ra `RaWire::is_low_latency` (time of the opening `OPTIONS`).
- svn-ra-api: the doc of `CommitEditor::close_file`.
- svn-delta: zlib level 5 for svndiff 1 sections (was 6). It only affects commit uploads.

## Gates (final head)
Head `483a80d` (includes `origin/main` 05b8955). `CARGO_INCREMENTAL=0`; svn 1.15.0 as oracle with
`OPENSVN_REQUIRE_SVN=1`.

| gate | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | clean |
| `cargo clippy -p opensvn --features real-engine --all-targets -- -D warnings` | clean |
| `cargo clippy -p svn-wc -p svn-client -p opensvn-crash --features failpoints --all-targets -- -D warnings` | clean |
| `cargo clippy -p opensvn --features failpoints --all-targets -- -D warnings` | clean |
| `cargo test --workspace --locked` | 1415 passed, 0 failed, 22 ignored |
| `cargo test -p opensvn --features opensvn/real-engine` | 487 passed, 0 failed |
| `cargo test -p svn-wc --features failpoints` | 244 passed, 0 failed, 1 ignored |
| `cargo test -p svn-client --features failpoints` (21 commit failpoint tests) | 214 passed, 0 failed |
| `cargo test -p opensvn --features failpoints` | 493 passed, 0 failed |
| DAV suite `real_engine_dav --ignored`, Apache 2.4.58 + mod_dav_svn **1.14.3**, HTTPS, Basic, authz | 20 passed, 0 failed |
| DAV suite, same with mod_dav_svn **1.15.0** | 20 passed, 0 failed |
| `svn-ra` `compat_write` + `compat_read --ignored`, 1.14.3 / 1.15.0 | 11 + 11 passed, 0 failed |
| crash suite, named failpoints (`opensvn-crash --features failpoints --ignored`) | 91 cases, 0 failed (commit 19/19) |

The svn-client tests compare against svn **1.15** and return early without it. Ubuntu 24.04 ships
1.14.3, so on a machine without 1.15 most of them pass without testing anything. Every run
above used svn 1.15.0, built from the tarball pinned in `docker/compose.yml` (SHA-512
verified), with `OPENSVN_REQUIRE_SVN=1`. The same build gave the 1.15 server: a second Apache
loading its `mod_dav_svn`/`mod_authz_svn`.

## Adversarial review
- **Data loss / wrong base**:
  - a delta is sent only after the base on disk was hashed and matched the recorded size and
    MD5;
  - the server additionally checks `X-SVN-Base-Fulltext-MD5` against its own base, and checks the
    result MD5 where `put-result-checksum` is announced;
  - tests cover a damaged and a truncated pristine falling back to the full text.
  - Copied and replaced files still go as full text, as before.
- **Commit atomicity**:
  - the MERGE is only sent after every background upload succeeded;
  - a failure, a cancellation or a dropped editor deletes the transaction as before;
  - background tasks are aborted when the editor is dropped or aborted.
  - Writes into the transaction are serialized (see FSFS above).
- **Post-commit crash safety**:
  - texts are synced before any row names them;
  - rows, locks and work items are in one transaction;
  - a re-run after a crash at any point converges: rows already there are authoritative, texts
    already in the store are verified and reused, and `file-commit` is idempotent.
  - Covered by the commit failpoint tests (21) and the crash suite.
  - The batch `file-commit` completion stores stat caches and deletes items in the same
    transaction, like the runner.
- **Harvest**: the read transaction holds only SQLite's shared lock and never writes; a write
  inside it fails instead of escaping it (test). Other processes' writes wait at most for the
  harvest (busy timeout 10 s).
- **TLS default-deny**: transport and trust code are untouched. Every DAV run used HTTPS with the
  test CA as an extra root, and the prompter rejects unknown certificates.
- **Credentials in logs**: no new logging. The benchmark prints timings and byte counts only.
- **svn 1.8–1.15 servers**:
  - deltas, `X-SVN-Base-Fulltext-MD5` and svndiff 0/1 are understood by every mod_dav_svn with
    HTTPv2 (1.8+);
  - svndiff 2 and the result checksum are only used when the server announces them;
  - the serialized writes are what ra_serf does on every version.
  - Tested against 1.14.3 and 1.15.0; 1.8–1.10 rely on the capability checks above.

## Crash suite
- Named failpoints on the final head: all 91 cases pass, including the 19 commit cases.
- Before the last merge of `origin/main` (without #138), 16 update and 9 cleanup cases failed
  ("unexpected EOF during chunk size line" while reading the update REPORT), and so did
  `origin/main` itself with the same seed. Commit cases passed on both. #138 (resumable
  interrupted updates) fixed them.
- An earlier SIGKILL run showed the same update/cleanup-only pattern; commit 7/7 passed.

## Not done here / follow-ups
- The VisualSVN-like environment of #134 (`scripts/test-env/visualsvn-like.sh`, Kerberos) was
  not run; `compare-svn-commit` reproduces its two commit cases on the local Apache.
- A file added with history and then modified is sent as full text. svn sends a delta against
  the copy source.
