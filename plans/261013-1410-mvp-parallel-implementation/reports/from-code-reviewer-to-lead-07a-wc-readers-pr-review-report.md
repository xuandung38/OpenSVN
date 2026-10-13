# PR #51 review: 07A A3 part 1 (LOCK/EXTERNALS/dav_cache/inherited_props readers, mixed-rev/not-present/excluded copy)

Date: 2026-10-13. Head `4bddfdb`, base `feat/07a-wc-lock` @ `81f3c46` (#35 head). I reviewed only #51's own diff (2 commits, 6 files, +572/-34) from a `git archive` scratch copy.

## Verdict: **REQUEST CHANGES** (small)
- The copy semantics now match svn 1.15.0 in all 8 of their oracle cases and in 3 extra variants of mine.
- The LOCK join is exactly svn's.
- One reader rejects valid svn working copies as corrupt: EXTERNALS rows with kind `unknown`/`symlink`. The fix is a few lines.

## Gates (scratch, `CARGO_BUILD_JOBS=2`, `OPENSVN_REQUIRE_SVN=1`)
- fmt OK.
- `clippy -p svn-wc --all-targets -D warnings` OK, also with `--features failpoints`.
- `cargo test -p svn-wc --features failpoints` all green against svn 1.15.0:
  - cache_readers_vs_svn 3/3
  - nodes_ops_vs_svn 22/22
  - wc_lock 12/12
  - schema_* and the rest green
- cargo deny OK.
- No svn-types or svn-ra-api change.

## Focus checks

| Focus | Result |
|---|---|
| LOCK cache by repos location | MATCHES svn. `nodes n JOIN lock l ON l.repos_id = n.repos_id AND l.repos_relpath = n.repos_path WHERE … n.op_depth = 0` is the same predicate as `STMT_SELECT_BASE_NODE_WITH_LOCK` / `STMT_SELECT_NODE_INFO_WITH_LOCK` (wc-queries.sql 1.15.0: lock joined only for op_depth 0). Working (copied) nodes therefore show no lock, as in svn. An INNER JOIN with `.optional()` gives `None` both for "no lock" and "no BASE row", which is fine for a reader. The real-svn test compares against `svn lock` / `svn info`. |
| EXTERNALS join | INNER JOIN on `repository` is safe (`repos_id` NOT NULL). Filters `local_relpath`, `def_local_relpath` and `strict_descendant` are correct. Ordering is by path. **Kind and presence validation is too strict** (IMPORTANT below). |
| dav_cache parsing | Decoded with `decode_props`, i.e. the same proplist skel svn writes via `svn_sqlite__bind_properties`. That is the same encoder as `properties`, which is already proven on real blobs (skel_real_blobs + every NODES `properties` comparison in the oracle). The empty-blob case returns empty (my earlier MODERATE is fixed). **Risk: low.** The only test is hand-written because `file://` never writes dav_cache. A real blob needs ra_serf (http). Add one from the P06 compat Apache fixture when available (MODERATE). |
| inherited_props | `parse_iprops` checked against a real svn blob (`svn propset inh … d`, then a `svn update` writing the iprop cache). Empty blob returns `Some([])`. Errors are wrapped as Corrupt with the path. OK. |
| Mixed-rev / not-present / excluded copy | MATCHES svn. Their oracle cases (all row-identical, plus `svn status`): copy-mixed, copy-mixed-nested, copy-not-present, copy-excluded, copy-mixed-file. My extra variants, all row-identical to svn: **switched child** (`svn switch --ignore-ancestry ^/e d/sub`, then `cp d dc`), **mixed revision plus a deleted grandchild** (`update -r1 d/sub; rm d/sub/g; cp d dc`), **excluded file child** (`--set-depth exclude d/f2`). The algorithm (a child whose revision, repos_id or expected repos path differs from its parent starts its own layer at its own depth, with a not-present placeholder in the parent's layer) is svn's layering rule. Planning still happens before any write. |
| `lock()` → `conn_guard()` rename | Safe. The rename is private. `with_read`/`with_write` were updated, and the remaining `.lock()` calls in `wc_lock.rs` are on `Mutex` fields (`lock_owner`, `owned_locks`, `lock_gate`). The crate and all tests compile; no public API changed except the new `WcDb::lock(&RelPath)`. |

## Findings

[IMPORTANT] EXTERNALS reader rejects valid rows with kind `unknown` or `symlink` — crates/svn-wc/src/db/externals.rs `from_row` (`other => Err(corrupt("unknown external kind"))`)
- svn 1.15.0 writes `unknown` for externals when upgrading pre-1.7 working copies. In upgrade.c ~2180-2200, `db_kind = svn_node_unknown` is passed to `svn_wc__db_upgrade_insert_external`.
- wc_db.c:3486 also has `svn_wc__db_external_add_symlink`.
- Any status or update walk calling `externals_under` on such a WC fails with `Corrupt`. That is a false corruption report on a working copy svn/TortoiseSVN handle fine.
- Fix: map `unknown` to `NodeKind::Unknown` (or `None`) and `symlink` to `NodeKind::Symlink`. Keep `Corrupt` only for truly unknown words.
- Add a test that fabricates an `unknown` row on an svn-created WC; it is a one-statement UPDATE, the same technique as the dav_cache test.

[MODERATE] `def_operational_revision` / `def_revision` are exposed as `Option<String>` — externals.rs `External`
- svn binds them with `svn_sqlite__bind_revnum` (integer). The TEXT affinity stores them as text, and svn reads them with `column_revnum`.
- Exposing `Option<u64>` would be the matching contract and would spare every caller from re-parsing.
- No test covers a pinned definition (`-r N` or `@peg`). Add one (`svn propset svn:externals '-r1 ^/e ext'`).

[MODERATE] dav_cache coverage relies on a hand-written blob. As argued above, the risk is low (same encoder as `properties`). Add a real ra_serf blob once the compat HTTP fixture exists, and keep the hand-written one.

[MODERATE] Naming: the public `WcDb::lock(&RelPath) -> Option<LockInfo>` (repository lock cache) sits next to `wc_lock_*` (working-copy write locks). Rename it to `repos_lock`/`cached_lock` to avoid callers confusing the two lock kinds. The external `presence` check (normal/excluded only) is fine per the schema comment, but should be revisited together with the kind fix.

## Positive (risk calibration)
- The copy layering is now general (mixed-rev, switched, not-present, excluded, deleted descendants) and row-identical to svn in 11 oracle shapes. It is the strongest piece of A2/A3 evidence so far.

## Plan follow-ups
- A3 readers: complete once the externals kind mapping and its test land. The pinned-revision test and the real dav_cache blob can follow in P06 integration.

## Unresolved questions
1. None blocking. `svn_types::NodeKind::Unknown` exists (model.rs), so the kind fix needs no svn-types contract change.

Status: DONE_WITH_CONCERNS
Summary: REQUEST CHANGES for one small fix (externals kind `unknown`/`symlink` reported as corrupt). LOCK join, iprops/dav_cache parsing and the mixed-rev/not-present/excluded/switched copy shapes all match svn 1.15.0, including 3 reviewer variants.

---

## Re-review (head `40a6ac5`, 2026-10-13)

**Verdict: APPROVE.**

- **EXTERNALS kind:** `symlink` → `NodeKind::Symlink`, `unknown` → `NodeKind::Unknown`. Any other word is still Corrupt. Tested on an svn-created WC with fabricated rows.
- **Revision fields:** `def_revision` and `def_operational_revision` are now `Option<u64>`, and non-numeric text is Corrupt. My probe set `def_revision='abc'` and got `Corrupt "external revision \"abc\" is not a number"`. svn stores the value as `text` (`typeof`).
- **Pinned revisions:** checked on rows written by real svn 1.15.0. `url@1` gives peg=1 and rev=1; `-r1 url` gives rev=1 and peg=None.
- **Rename:** `WcDb::lock` is now `repos_lock`; no other callers.
- **Gates** (`CARGO_BUILD_JOBS=2`, `OPENSVN_REQUIRE_SVN=1`): fmt, clippy (with and without failpoints), `cargo test -p svn-wc --features failpoints` and deny all OK; cache_readers 5/5, nodes 22/22, wc_lock 12/12.
- **Remaining (non-blocking):** add a test for the Corrupt path (my probe above can be copied), and a real ra_serf `dav_cache` blob once the P06 HTTP fixture exists.

Status: DONE

---

## #66 (facade for 07B: `wq_record_and_fetch_next`, `read_repos_info`), 2026-10-13
Scope: only #66's own commits (`f9fd206`, `bc61b61`; `6c28a60` adds only main merges, #61). #58's test-helper migration is excluded.

**Verdict: APPROVE.**

- **`wq_record_and_fetch_next` is a single txn.** Delete, fileinfo and fetch-next all run in one `with_write` (`BEGIN IMMEDIATE`). svn 1.15.0 runs `wq_fetch_next` (delete, then `SELECT … ORDER BY id LIMIT 1`) and `wq_record` inside one `SVN_WC__DB_WITH_TXN`. Our step order (record before fetch) differs but is equivalent inside one txn.
- **Failpoint tests are real.** `wc.wq.after_delete` and `wc.wq.after_fileinfo` each inject Err. The tests check that both the delete and the fileinfo rolled back (2 items remain, `translated_size` None), and then that a successful pass returns the next item and leaves 1.
- **Fileinfo on a missing node** errors and rolls everything back, so the completed item replays. This matches svn's `SVN_ERR_ASSERT(affected_rows == 1)` in `db_record_fileinfo`.
- **[MODERATE] Single path only.** The API records at most one path, while svn takes a `record_map` hash. That is enough for today's handlers (file-install/translate each record one path). If a 07B handler ever records several, the API must take a slice.
- **`read_repos_info` matches svn.** Their `repos_info_vs_svn` oracle passes. My variants are all equal to `svn info` (root URL, UUID, relative URL):
  - switched subtree (`switch --ignore-ancestry ^/other d/sub`): `d/sub`, `d/sub/inner`, `d/sub/inner/h`, plus a file added inside the switched subtree;
  - moved file `d/f1 → d/f1m`, both source and destination;
  - moved dir `other → o2`, i.e. source child `other/inner/h`, `o2`, `o2/inner/h`.
- **`copyfrom` on a moved node** is reported as `(d/f1, 1)`. svn shows "Moved From"; the docs should say that moves also appear as copyfrom.
- **Failpoint macro.** Full paths (`svn_types::SvnError` / `ErrorCode`) plus `use failpoint;` make it usable from submodules regardless of imports. It compiles cleanly in both feature configs and is still compiled out without `failpoints`.
- **Gates** (`CARGO_BUILD_JOBS=2`, `OPENSVN_REQUIRE_SVN=1`, on `bc61b61`): fmt, clippy (with and without failpoints), `cargo test -p svn-wc --features failpoints` (all suites green) and deny all OK.
