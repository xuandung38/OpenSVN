# PR #33 review — feat/07a-wc-nodes (A2: NODES model + working-layer ops + txn failpoints)

Date: 2026-10-13. Head `41f2359` (base `feat/07a-wc-schema` @ `9033110`). Reviewed from a `git archive` scratch export; worktree untouched.

## Verdict: **REQUEST CHANGES**
I added svn-oracle probes using the PR's own `check()`/`dump_rows` harness. 4 of 5 show row divergence from svn 1.15.0, and 2 of those leave a WC that svn/TortoiseSVN will interpret wrongly: stale BASE subtree after committing a replacement, and a deleted child that silently comes back in a copy. The PR's happy-path oracle is a real comparison, but it does not cover these shapes.

## Gates (scratch, rustc 1.98.1)
- fmt OK; `clippy -p svn-wc --all-targets -D warnings` OK, also with `--features failpoints`; `cargo test -p svn-wc` green; `--features failpoints --test wc_failpoints` green; `cargo deny` OK (new dep `fail =0.5.1`, optional, feature-gated).
- No changes in svn-types/svn-ra-api. Root `Cargo.toml` adds only the `fail` workspace dep; rusqlite gains the `functions` feature.

## Focus checks
| Focus | Result |
|---|---|
| Single txn per write + work items in same txn | MET for every per-node API: `write_with_work` runs row changes and `INSERT INTO work_queue` inside one `BEGIN IMMEDIATE`. The failpoint test proves before-commit gives nothing and after-commit gives both (rows + item). Gap: commit post-processing is one txn **per path** (see IMPORTANT-5). |
| svn-oracle row comparison is real | YES. Every NODES column except `translated_size`/`last_mod_time`/`dav_cache` is compared, plus ACTUAL_NODE and PRISTINE rows (incl. refcount), plus `svn status --xml -v`. Weakness: the scenarios are happy-path only, and the oracle skips silently when svn < 1.15 / absent (same CI issue as #29). |
| op_depth semantics add/delete/copy/move/revert/commit | PARTIAL. Simple shapes match svn; the compound shapes below diverge. |
| presence transitions | normal→base-deleted, add over base-deleted/not-present, delete→not-present on commit: OK in simple cases. Copy drops non-present children (see IMPORTANT-2). |

## Findings

[IMPORTANT] Commit of a replaced directory leaves the old BASE subtree and base-deleted rows — crates/svn-wc/src/db/ops.rs:666-747 (`commit_node`, op_depth>0 branch) — Repro (`svn rm d; mkdir d; svn add d; commit` vs ours): svn ends with only the new `d` at op_depth 0 and pristine refcounts 0. Ours keeps `d/f1, d/f2, d/sub, d/sub/g` at op_depth 0 **and** `base-deleted` rows at op_depth 1, with refcount 1. After the commit, svn/TSVN would show these as scheduled deletes of paths that no longer exist in the repository. Fix: when the committed layer replaces BASE (a BASE row exists at path), first remove the BASE subtree and every row at op_depth >= d in scope (with `detach_moves`), then insert the new BASE rows. Add a `commit-replace` oracle test.

[IMPORTANT] `op_copy` drops deleted / not-present / excluded children instead of carrying them — ops.rs:399-402 — Repro (`svn rm d/f2; svn cp d dc`): svn writes `dc/f2` as `normal` at op_depth 1 **plus** `base-deleted` at op_depth 2 (refcount 2). Ours writes no `dc/f2` row at all. The commit editor then sends a plain copy of `d`, so the repo gets `dc/f2` back while the WC has no record of it: the user's delete is silently lost. svn also carries `not-present`/`excluded`/`server-excluded` children of the source as `not-present`/`excluded` rows in the copy layer. Fix: mirror the svn copy (`STMT_INSERT_WORKING_NODE_COPY_FROM` + the delete layers of the source subtree, re-based to dst depth). Add oracle tests for a deleted child, a not-present child (after a committed delete in a mixed-rev parent) and an excluded child.

[IMPORTANT] Recursive revert of a BASE node ignores working layers below it — ops.rs:483-503 — Repro (`svn add d/new; svn revert -R d`): svn removes the add, ours leaves `d/new` op_depth 2 `normal`. `op_revert(.., recursive=true)` with `cur.op_depth == 0` only clears ACTUAL. Fix: when recursive, remove every op root in the subtree (rows with op_depth >= relpath_depth(row's op root) inside scope), with `detach_moves`. Otherwise the P11 caller must iterate, and that needs to be documented and tested.

[IMPORTANT] Non-recursive revert of an op root silently reverts deeper operations — ops.rs:485-500 — Repro (`svn cp d dc; svn rm dc/f1; svn revert dc`): svn refuses with `E155038 Can't revert '…/dc' without reverting children`. Ours returns `Ok` and deletes all op_depth >= d rows, including the separate child delete. The `recursive` flag is ignored on this branch. Fix: if `!recursive` and there are rows in strict descendants with op_depth > d, return the svn-equivalent error.

[IMPORTANT] Commit post-processing is not atomic across paths — node_api.rs:186-191 (`commit_node` = one txn per path) — If the app crashes after a successful server commit while bumping, some nodes are at new_rev and others are still scheduled add/copy. The next commit then fails ("already exists") or re-sends, and status is wrong until a manual fix. Fix: provide `commit_nodes(&[(RelPath, CommitInfo)], work)` running in one `with_write` txn, or queue a single "post-commit" work item first so cleanup can finish it. Document the requirement for P11.

[MODERATE] Moving a copied (non-moved) node marks the destination `moved_here=1` with no `moved_to` source — ops.rs:463-473 — Repro (`svn cp top.txt c.txt; svn mv c.txt m.txt`): svn writes `m.txt` as a plain copy (mh=NULL). Ours sets mh=1, but the `moved_to` UPDATE matches no row (the copy layer was deleted, no base-deleted row was created). This dangling `moved_here` can confuse svn's move tracking (tree-conflict resolution and `svn status` "moved from"). Fix: only set moved_here when the source is a BASE node or the root of a move; otherwise do a plain copy, as svn does.

[MODERATE] `decode_props` errors on a zero-length blob — crates/svn-wc/src/db/nodes.rs (`decode_props`, doc says "an empty blob or () is an empty property list") — `Skel::parse(b"")` fails with Corrupt. svn's `svn_sqlite__column_properties` treats zero-length as NULL (`sqlite3_column_blob` returns NULL). So a valid svn WC could fail to read here. Fix: map an empty blob to None or empty, and add a test.

[MODERATE] Silent CI skip of all oracle tests — tests/nodes_ops_vs_svn.rs `check()` uses `skip_without_svn_1_15()` — same issue as #29: CI has no svn 1.15, so none of the A2 compatibility evidence runs in CI. Fix: install svn 1.15 in one CI job, or run the Docker oracle there, and fail when `OPENSVN_REQUIRE_SVN=1`.

[MODERATE] `op_copy` of locally added nodes inside the source subtree returns "not supported yet" as `ErrorCode::Internal` — ops.rs:409-416 — Fine for scope, but use a user-facing code (e.g. WcFormatUnsupported/Unsupported) so the UI does not show it as an internal bug.

## Positive (risk calibration)
- `IS_STRICT_DESCENDANT_OF` port is exact (range form, `X'FFFF'`); `relpath_depth` registered DETERMINISTIC, NULL for non-text, like svn.
- Single-txn and work-queue atomicity are proven by fault injection rather than asserted.

## Plan follow-ups
- A2 M3 nodes: not complete until IMPORTANT 1-4 are fixed with oracle tests (copy-with-deleted-child, commit-replace, revert -R with descendant ops, non-recursive revert refusal, move-of-copy).
- P11 dependency: batch commit API (IMPORTANT-5).

## Unresolved questions
1. Is P11 expected to drive recursive revert per op root, or should `op_revert(recursive)` own it? The answer decides whether IMPORTANT-3 is a defect or a missing doc and contract.
2. Are mixed-revision copies (children at a different revision than the copy root) in A2 scope? Not covered by any test.

Status: DONE_WITH_CONCERNS
Summary: REQUEST CHANGES. Transactions and the oracle are sound, but 4 reproduced op_depth/commit divergences from svn 1.15.0, two of which leave a WC that svn/TortoiseSVN interpret wrongly.

---

## Re-review (head `2be4ea9`, base `ca195e0`, 2026-10-13)

**Verdict: REQUEST CHANGES (one-line fix plus one test).** Every earlier finding is fixed with permanent oracle tests. My new probe found one remaining divergence in the replaced-directory commit fix.

Gates (scratch, `CARGO_BUILD_JOBS=2`): fmt OK; `clippy -p svn-wc --all-targets -D warnings` OK, also with `--features failpoints`; `OPENSVN_REQUIRE_SVN=1 cargo test -p svn-wc --features failpoints` green (nodes_ops_vs_svn 17/17 against svn 1.15.0); cargo deny OK.

| Earlier finding | Status | Evidence |
|---|---|---|
| Copy drops deleted children | FIXED | `op_copy` takes the row at or below the source's op depth and re-creates its delete layers re-based to dst. Tests `copy-with-deleted-child` and `copy-of-copy-with-deleted-child` are row-identical to svn. Not-present/excluded/incomplete children, locally added nodes inside the source and mixed-revision sources are now an explicit `WcFormatUnsupported`, planned before any write (no partial copy). Fail-safe, acceptable for MVP. |
| `revert -R` of BASE ignores descendant layers | FIXED | Recursive branch deletes op_depth > 0 in scope with `detach_moves`. Oracle test passes. |
| Non-recursive revert of op root with child ops | FIXED | Refused with svn_code 155038 "can't revert … without reverting children". Oracle test passes. (Nit: `ErrorCode::Internal`. A user-facing code would map better in the UI.) |
| Replaced-dir commit leaves old BASE | FIXED for replace-by-add | `commit_of_a_replaced_directory_drops_the_old_base_tree` is row-identical. |
| Commit not atomic across paths | FIXED | `commit_nodes(&[(path, info)], work)` runs one `with_write` txn; `commit_node` delegates to it. |
| Move of a copy sets moved_here | FIXED | moved_here only when the source has a lower live layer. Oracle test passes. |

**[IMPORTANT] A replace-by-copy commit wipes delete layers that belong to the new copy** — crates/svn-wc/src/db/ops.rs:826 (`… presence = 'base-deleted' AND op_depth >= ?3`).
- Repro (permanent oracle shape): `svn rm e; svn cp d e; svn rm e/f2; commit`, compared with `op_delete(e); op_copy(d,e); op_delete(e/f2); commit_nodes([(e),(e/f2)])`.
- svn ends with `e/f2` at op 0 `not-present`@2, pristine ref 1.
- Ours ends with `e/f2` at op 0 **`normal`** (checksum, cr=2), ref 2. The WC claims a file the repository just deleted.
- Cause: the replacement cleanup also deletes the base-deleted row at op_depth 2, which records the separate delete *inside the new copy*. The later `commit_node(e/f2)` then sees a plain BASE node.
- The old BASE subtree's own delete layers can only sit at exactly `cur.op_depth`, because `op_delete(e)` already removed every deeper layer of the old tree.
- **Fix: `op_depth = ?3`.** Verified in scratch: with that one change my probe is row-identical to svn and all 17 existing oracle tests still pass.
- Add the probe as a permanent test (`commit-replace-by-copy-with-deleted-child`).

Status: DONE_WITH_CONCERNS. Approve once the one-line fix and its test land.

---

## Re-review 2 (head `70928fc`, base `main` after #29 merged, 2026-10-13)

**Verdict: APPROVE.** Root-only commits now match svn 1.15.0 row for row in every variant I tried. One IMPORTANT API-contract follow-up must land before P11 starts calling `commit_nodes`. It is fail-safe today: the batch rolls back atomically, and nothing calls this code yet.

Gates (scratch, `CARGO_BUILD_JOBS=2`, `OPENSVN_REQUIRE_SVN=1`): fmt OK; `clippy -p svn-wc --all-targets -D warnings` OK, also with failpoints; `cargo test -p svn-wc --features failpoints` all green (nodes_ops_vs_svn 18/18 against svn 1.15.0); cargo deny OK. The E155038 refusal now carries `WcFormatUnsupported` (my nit is fixed), and the IOERR hint text has been added.

Oracle probes: svn commit vs `commit_nodes`, compared with `dump_rows` and `svn status` on our WC.

| Probe | Batch passed | Result |
|---|---|---|
| `rm e; cp d e; rm e/f2` (replace, file child) | `[e]` | ROWS EQUAL, status clean |
| replace, dir child `rm e/sub` | `[e]` | ROWS EQUAL, status clean |
| replace, nested file `rm e/sub/g` | `[e]` | ROWS EQUAL, status clean |
| plain copy `cp d dc; rm dc/sub` | `[dc]` | ROWS EQUAL, status clean |
| replace, file child | `[e, e/f2]` | **Err** "'e/f2' is not-present and cannot be committed"; whole batch rolled back |
| plain copy, dir child | `[dc, dc/sub]` | **Err** "'dc/sub' is not-present and cannot be committed"; rolled back |

[IMPORTANT, follow-up before P11] `commit_nodes` rejects the natural committable list — crates/svn-wc/src/db/ops.rs `commit_node` (the "cannot be committed" guard), plus the doc on `WcDb::commit_nodes`.
- svn's harvester lists both the copy root and each delete inside it as committables.
- The new code folds the child deletes into not-present rows while processing the root. The child entry that follows then fails, so the batch aborts.
- The one-line fix I verified earlier handled this batch correctly; this change regressed it.
- If P11 passes the full list, a successful server commit leaves the WC un-bumped (atomic, but every later commit then fails with "already exists").
- The doc "for a committed delete pass only the root of the deletion" is misleading here, because `e/f2` *is* the root of its own deletion.
- Fix, either:
  - (a) make `commit_node` idempotent: if the path's current row is BASE `not-present` at `new_rev`, created by an ancestor in the same batch, return Ok (preferred); or
  - (b) document precisely "deletes inside a committed copy/replacement are folded by the copy root; do not list them".
- Add the `[root, child]` variant to the oracle either way.

Non-blocking: the not-present conversion matches descendants with `LIKE ?2 || '/%' ESCAPE '\'` instead of the existing `strict_descendant` helper. It is correct (escaping done, `case_sensitive_like=1`), but it is a second implementation of the same predicate.

## #35 merge check
`81f3c46` contains `70928fc` (`git merge-base --is-ancestor`). The diff `70928fc..81f3c46` is exactly #35's own 4 files (wc_lock.rs, tests/wc_lock.rs, mod.rs wiring, dev report), with no conflict markers. GitHub reports MERGEABLE. Gates on `81f3c46` are all green (wc_lock 12/12, nodes 18/18). The #35 verdict stays **APPROVE**.

Status: DONE_WITH_CONCERNS. #33 and #35 can go to the merger; track the `commit_nodes` idempotency/doc follow-up for P11.

## #55 (idempotent `commit_node`, head `49ce46e`, 2026-10-13)
**APPROVE.**
- The `[e, e/f2]` oracle test is row-identical to svn 1.15.0. My extra case, plain copy `[dc, dc/sub]`, is also row-identical.
- The skip fires only when the current row is BASE (op_depth 0), not-present, and at exactly `new_rev`. Probed:
  - a missing path in the batch still fails with PathNotFound and rolls the whole batch back;
  - a child at a different revision still fails ("not-present and cannot be committed").
- Gates (`CARGO_BUILD_JOBS=2`, `OPENSVN_REQUIRE_SVN=1`): fmt, clippy (with and without failpoints), `cargo test -p svn-wc --features failpoints` and deny all OK.
