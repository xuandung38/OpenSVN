# PR #29 review — feat/07a-wc-schema (track A, milestone A1)

Date: 2026-10-13. Reviewed head `9033110` (re-checked via `gh pr view`; earlier pass on `13a9ebb`, delta = create-ownership fix + test tweaks). Scratch copies only; worktree untouched (note: worktree is now checked out on `feat/07a-wc-nodes` by dev-07a).

## Verdict
**APPROVE WITH FOLLOW-UPS (merge OK for A1).** No CRITICAL. 2 IMPORTANT issues are robustness/coexistence defects in the open path that must be fixed before any user-facing open (A2/P11), not blockers for landing the schema/codec. Fix IMPORTANT-1 in this PR if cheap (small change).

## Gates (head 9033110, scratch copy, rustc 1.98.1)
- `cargo fmt --all --check` OK; `cargo clippy --workspace --all-targets --locked -D warnings` OK; `cargo test --workspace --locked` all green; `cargo deny check` OK (only "unmatched MPL-2.0 allowance" warning).
- Opt-in `OPENSVN_TEST_DOCKER_SVN=1 created_schema_matches_svn_1_14_5_in_docker` PASS (svn 1.14.5, alpine:3.22).
- No changes in `crates/svn-types`, `crates/svn-ra-api`, root `Cargo.toml`/`Cargo.lock` from this PR's own commits (contract untouched).
- Lib code: no unwrap/expect/unsafe; doc comments present; `#[lints] workspace = true`.

## A1 acceptance
| Item | Result | Evidence |
|---|---|---|
| Skel codec canonical (implicit iff 0<len<100, letter start, no ws/paren-class; `[ ]` paren class; explicit `<len> <bytes>`; lists single-space) | MET | Re-derived against skel.c 1.14.5 (`use_implicit`, `getsize`, `explicit_atom`, `list`); hostile vectors: `u64::MAX`/overflow lengths rejected, `\v` not space, `\t` accepted as length separator, `[a]`/`(a]b)` rejected (= svn). Real-blob round-trip tests pass. |
| Format 31 schema structurally = svn 1.14.5 / 1.15.0 | MET | Test passes for both; extra reviewer check: comment/ws-normalized `sqlite_master.sql` text of all 32 objects identical to svn 1.15.0 checkout (0 diffs) — stronger than the PR's PRAGMA-based compare. |
| open / find_wcroot | MET | nearest `.svn/wc.db`, pre-1.7 `entries` → WcFormatUnsupported. |
| 31 rw / 32 ro / others rejected untouched | MET | RO probe before RW reopen; byte-identical file after rejection (tested). Reviewer probe: opening an existing format-31 WC with stat1 present + a read leaves every `.svn/*` file byte-identical and wc.db mtime unchanged. |
| Pragmas don't change journal mode | MET | header bytes 18/19 stay 1/1; `journal_mode` reads `delete` from fresh connection. No WAL anywhere. |

## Findings

[IMPORTANT] Read-only format probe cannot open a WC with a hot journal (after svn/TortoiseSVN crash mid-write) — crates/svn-wc/src/db/mod.rs:255 (probe `OpenMode::ReadOnly`) + crates/svn-wc/src/db/connection.rs:39 (ReadOnly→PermissionDenied) — Reproduced: child process writes in `BEGIN IMMEDIATE` with spilled cache then `os._exit`; `WcDb::open` fails for BOTH format 31 and 32 with `PermissionDenied: "wc.db: attempt to write a readonly database"` (SQLITE_READONLY_ROLLBACK). `svn status` on the same WC rolls the journal back and succeeds. Data is not damaged, but OpenSVN refuses the WC with a misleading code until svn/TSVN touches it. Recommendation: probe with `SQLITE_OPEN_READ_WRITE` (no CREATE) when the file is writable — opening RW and reading `user_version` writes nothing except SQLite's own hot-journal rollback, which is required recovery and what svn does; keep RO only when the file is not writable. For format 32 use the same RW connection but keep API-level write refusal (`with_write`). Map `SQLITE_READONLY_ROLLBACK`/`READONLY_RECOVERY` to a distinct "needs cleanup" message if RO must stay. Add a test with a hot journal.

[IMPORTANT] `ensure_statistics` on an existing WC is non-atomic and can leave permanent partial state — crates/svn-wc/src/db/schema.rs:67-90 (called at mod.rs:267) — svn runs `verify_stats_table` inside `SVN_SQLITE__WITH_LOCK4` (one savepoint, wc_db_wcroot.c 1.15.0:540). Here `ANALYZE`, `DELETE`, 9×`INSERT`, `ANALYZE`, `DROP sqlite_stat4` run as ~13 autocommit transactions. If TSVN grabs the write lock between them: `sqlite_stat1` exists but empty (svn never re-installs because the table exists → planner hints lost forever for TSVN/svn on that WC) and/or `sqlite_stat4` left in a user's WC. Also measured: under a held RESERVED lock `WcDb::open` blocks 10.7 s (busy timeout) and then silently proceeds. Recommendation: wrap install in `BEGIN IMMEDIATE … COMMIT` (rollback on any error, then ignore like svn); consider a short busy timeout (e.g. 0–250 ms) just for this optional hint install so open never stalls 10 s behind TSVN; `tracing::debug!` the ignored error instead of `let _ =`.

[MODERATE] `reverse_unordered_selects = ON` in production — crates/svn-wc/src/db/connection.rs:80 — svn only sets it under `#ifdef SVN_SQLITE_REVERSE_UNORDERED_SELECTS` (sqlite.c 1.14.5:1180), a debug-only define; the lead note (table row) is wrong. No file impact, but release builds will return un-ORDERed rows in reverse vs svn (status/list ordering drift in A2+). Recommendation: `#[cfg(debug_assertions)]` or test-only; correct `from-lead-svn-sqlite-settings-note.md`.

[MODERATE] Format TOCTOU between probe and RW reopen — crates/svn-wc/src/db/mod.rs:253-264 — format is read on the probe connection, the probe is closed, then a new RW connection is opened and `self.format` is trusted for the WcDb lifetime. A concurrent `svn upgrade --compatible-version=1.15` in that window yields a format-32 WC treated as writable 31. Recommendation: re-read `user_version` on the final connection (cheap) and fail if it differs; ideally re-check inside `with_write` after `BEGIN IMMEDIATE` (one PRAGMA) since svn 1.15 can upgrade a WC while OpenSVN holds it open.

[MODERATE] svn-backed acceptance tests skip silently in CI — crates/svn-wc/tests/schema_support/mod.rs (`skip_without_svn`, `skip_without_svn_1_15`) + .github/workflows/ci.yml (no svn install step) — schema equality, 31 rw / 32 ro and svn-interop tests return early and report "ok" when svn is missing/old (macos-15 runner has no svn; ubuntu image svn, if any, is 1.14.x → format-32 tests skip). A1 is therefore not enforced by CI. Recommendation: install subversion in the ubuntu job (ideally 1.15 via Docker/apt backport) and add `OPENSVN_REQUIRE_SVN=1` that turns skips into failures there.

[MODERATE] Structure test now ignores `sqlite_stat4` — crates/svn-wc/tests/schema_support/mod.rs:~158 (added in 9033110) — this removed the only guard that OpenSVN-created WCs don't carry `sqlite_stat4`. Reviewer probe confirms create and stat re-install currently drop it, but a regression would go unnoticed. Recommendation: keep the skip for the svn side only, and assert `sqlite_stat4` absent on the OpenSVN side; also add the normalized-SQL-text comparison (catches FK actions/CHECK/COLLATE/index sort order not covered by PRAGMA table_info/index_info).

[MODERATE] Apache-2.0 attribution incomplete — crates/svn-wc/src/db/sql/create-schema-format-31.sql:1-4, install-schema-statistics.sql:1-4 — upstream ASF license header of wc-metadata.sql was replaced by a paraphrase; repo has no LICENSE/NOTICE file. Apache-2.0 §4(c)/(d) require retaining the source's attribution notices and carrying NOTICE content. Recommendation: keep the original ASF header block verbatim above the "Derived from … modified: …" line; add root `LICENSE` (Apache-2.0) and `NOTICE` with "Apache Subversion — Copyright The Apache Software Foundation" entry for the derived SQL (and skel grammar port).

[MODERATE] Missing SETTINGS row maps to Internal — crates/svn-wc/src/db/mod.rs:278-285 — `query_row` with no row → `QueryReturnedNoRows` → `ErrorCode::Internal`; svn treats it as corrupt (`step_row`). Recommendation: `.optional()` + `ok_or(Corrupt "missing row in SETTINGS")`.

[MODERATE] Skel `MAX_DEPTH = 512` is more than needed and stack-sensitive in debug — crates/svn-wc/src/skel.rs:27 — measured: depth-512 parse needs ~600–768 KB stack in debug (~1.4 KB/level), <128 KB in release; aborts (not panics) on a 256 KB thread. Safe on std 2 MB threads, but hostile blob + small custom stack (some async runtimes/worker pools) = process abort. Real skels nest < 10. Recommendation: lower to 64 (or make parse iterative). Allocation is bounded by input length (max ~16× amplification for `(()()…)`), OK.

## Edge cases checked (no issue)
- Rejected formats (29/30/33): file untouched. Existing format-31 open: no write when stat1 present; journal file content unchanged.
- `PRAGMA journal_mode=TRUNCATE` only on RW connections; `synchronous=FULL` deliberate (documented deviation from svn OFF), compatible with svn readers.
- `create` (9033110): non-recursive `mkdir .svn` gives single-owner semantics; pre-1.7 `.svn/entries` and foreign `.svn` no longer overwritten (my earlier finding on 13a9ebb is fixed and tested, incl. 4-thread race test).
- Skel: length overflow checked against remaining bytes before allocation; empty input, unterminated list, trailing garbage (strict `parse`) rejected; `parse_prefix` matches svn's prefix semantics.
- `Mutex<Connection>` + `NO_MUTEX`: correct (Connection is Send, not Sync); poison recovery OK.
- `with_write` holds the in-process mutex while `BEGIN IMMEDIATE` waits up to 10 s — documented; acceptable for A1.

## Plan follow-ups (no plan edits made)
- A1 (M1 skel + M2 schema read/create 31) appears complete once IMPORTANT-1/2 are fixed or tracked for A2.
- A2 should carry: format re-check in `with_write`, `relpath_depth` + NFD `like()` (dev report §6), CI svn install.

## Unresolved questions
1. Does the ubuntu-24.04 runner image ship svn (and which version)? Decides whether any svn-backed test actually runs in CI today.
2. Lead decision: should OpenSVN install stat1 hints on existing WCs at all (svn parity) or stay strictly read-until-write for TSVN coexistence? Parity is defensible if done atomically.
3. Should the lead note's `reverse_unordered_selects` row be corrected (it is debug-only in svn)?

Status: DONE_WITH_CONCERNS
Summary: A1 acceptance met and all gates green at 9033110; merge OK. Two IMPORTANT open-path issues (hot-journal RO probe fails; non-atomic stat1 re-install on existing WCs) should be fixed before user-facing open.

---

## Re-review (head `ca195e0`, 2026-10-13)

**Verdict: APPROVE.** Both IMPORTANT items are fixed and tested. All MODERATE items are fixed except CI provisioning of svn (non-blocking).

Gates (scratch, `CARGO_BUILD_JOBS=2`): fmt OK; `clippy --workspace --all-targets -D warnings` OK; `clippy -p svn-wc --features failpoints` OK; `OPENSVN_REQUIRE_SVN=1 cargo test -p svn-wc --features failpoints` all green (every svn-backed test ran against svn 1.15.0, none skipped); cargo deny OK. I did not re-run the full workspace test because of machine load. The PR's own changes outside svn-wc are only the CI step; the svn-ra-api diff comes from the already-merged #37 on main.

| Earlier finding | Status | Evidence |
|---|---|---|
| IMPORTANT hot journal / RO probe | FIXED | `open_existing` opens READ_WRITE (no CREATE), falls back to RO on PermissionDenied, and uses `is_readonly(MAIN)` to detect a silently downgraded RW open. Test `opens_a_working_copy_with_a_hot_journal_like_svn` uses a real spilled journal: the open rolls back and a write works afterwards. A write-protected file opens RO (`is_writable()=false`). |
| IMPORTANT non-atomic stat1 install | FIXED | `BEGIN IMMEDIATE` … `COMMIT` with ROLLBACK on any error, busy timeout 100 ms for this step then restored to 10 s, failure logged at debug. Test: under a held RESERVED lock, open returns in under 3 s with no `sqlite_stat%` tables; the next open installs ≥ 8 rows and no stat4. |
| reverse_unordered_selects | FIXED | `cfg!(debug_assertions)` only. |
| Format TOCTOU | FIXED | `user_version` re-read on the final connection (mismatch gives WcBusy "retry") **and** inside every `with_write` txn after BEGIN IMMEDIATE (gives WcFormatUnsupported). Tested. |
| Missing SETTINGS row | FIXED | Gives Corrupt "missing row in SETTINGS"; tested. |
| MAX_DEPTH | FIXED | 64. |
| stat4 guard | FIXED | Ignored on the svn side only (`structure_of_svn_wc`); OpenSVN side asserts no stat4. |
| Apache-2.0 attribution | FIXED | Original ASF header kept verbatim plus a "Modified by OpenSVN" line in both .sql files; root `NOTICE` added (via #39). |
| CI svn skip | PARTIAL | `OPENSVN_REQUIRE_SVN=1` now turns skips into failures, but ci.yml neither installs svn nor sets the variable, so CI still skips silently. Track for P06/P13; not blocking. |

**New mapping SQLITE_IOERR_DELETE (2570) / SQLITE_IOERR_LOCK (3850) → WcBusy.** The constants are correct (`SQLITE_IOERR | n<<8`).
- IOERR_DELETE → WcBusy: **correct**. On Windows, deleting a journal another process holds open fails with a sharing violation; SQLite's `winRetryIoerr` has already retried by then. The cause is transient (an AV/indexer, or a DELETE-mode peer racing an svn/TSVN TRUNCATE-mode connection that keeps the journal open). Our own connections never delete the journal in TRUNCATE mode, so this mostly covers rare paths. Low risk either way: both codes abort the operation with nothing written.
- IOERR_LOCK → WcBusy: **acceptable, slightly optimistic**. On unix it usually means the filesystem cannot lock (NFS/SMB without lock support), which is persistent rather than contention. Real lock contention arrives as SQLITE_BUSY. Suggest (non-blocking) keeping WcBusy but adding "if this persists, the file system may not support locking (network share)" to the message, so a retry loop has an exit hint.

Status: DONE. Merge OK.
