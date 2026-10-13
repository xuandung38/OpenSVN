# PR #49 review — 07B B2: work queue (M6) + conflict skels (M7)

Reviewer: code-reviewer. Date: 2026-10-13. Head `6e56072`; reviewed only its own diff (`6ee6090..6e56072`): `workqueue/{mod,item,error,runner,tests}.rs`, `conflicts.rs`, `conflicts/tests.rs`, `tests/compat/scenarios/wc/conflicts/*`.
Upstream compared: `libsvn_wc/workqueue.c` (1.14.5 + 1.15.0), `conflicts.c`/`conflicts.h` 1.15.0, `wc_db.c` 1.15.0 (`read_node_install_info`), `libsvn_subr/path.c` (URI table). Checked against real queue blobs from svn 1.15.0: made the WC directory read-only, ran `svn up` / `svn revert` / `svn propset`, and read `WORK_QUEUE.work` from wc.db.

## Verdict: REQUEST CHANGES

1 CRITICAL (an interrupted svn revert/merge leaves a queue OpenSVN cannot run), 2 IMPORTANT, several MODERATE. The codec, run-loop semantics and conflict-skel codec are correct.

## Gates (macOS, `CARGO_BUILD_JOBS=2`; CI blocked by billing)
| Gate | Result |
|---|---|
| fmt | OK |
| clippy `-p svn-wc --all-targets --all-features -D warnings` | OK |
| `cargo test -p svn-wc --locked` | OK (79 unit + golden/failpoint suites) |
| `cargo deny check` | OK |
| unwrap/expect/unsafe in lib | none |
| Owned paths / contracts | Only 07B paths; no Cargo, `svn-types` or `svn-ra-api` changes in the own diff |
| Windows paths | **Unverified**: no CI and no Windows target locally |

## Real svn 1.15.0 blobs (evidence)
Captured from wc.db after a forced failure:
```
(file-install b.txt 1 0 1 1 19 .svn/tmp/svn-bCQBU2)   <- interrupted `svn revert`
(file-remove 19 .svn/tmp/svn-bCQBU2)
(file-remove d/f)   (dir-remove d)                     <- interrupted `svn up` (deleted dir)
(sync-file-flags a.txt)                                <- interrupted `svn propset svn:executable`
(file-remove c.txt)                                    <- interrupted `svn up` (incoming move)
```
I ran a scratch test (not committed): `WorkItem::decode(..).encode()` reproduces all 6 byte for byte, including the explicit-length atoms `1 0` and `19 .svn/...`. **Arity and bytes match svn.**

## CRITICAL

### C1. The runner refuses svn's own `.svn/tmp` paths, so an interrupted `svn revert` or merge can never complete
`runner.rs::abs()` sends every relpath through `svn_types::safe_join`, which rejects any `.svn` segment. svn 1.15 writes items whose paths are **inside the admin dir**: revert installs from `.svn/tmp/svn-XXXX` and then removes it, and merge/resolve use `file-move .svn/tmp/... target` and `file-translate` with temp sources. Scratch test feeding the real revert queue:
```
Err(ItemFailed { id: 1, skel: "(file-install b.txt 1 0 1 1 19 .svn/tmp/svn-bCQBU2)",
  source: UnsafePath(".svn/tmp/svn-bCQBU2: administrative directory in .svn/tmp/svn-bCQBU2") })
```
Impact: every WC that svn or TortoiseSVN left mid-revert or mid-merge is stuck in OpenSVN with "needs cleanup". OpenSVN's own revert/merge (11U/11C) will need these items too. The existing codec test even encodes `source: ".svn/tmp/x"`, but no runner test executes it.
Fix: give the runner two resolvers.
- WC paths go through `safe_join`, as now.
- Admin temp paths match exactly `ADM_DIR + "/tmp/" + <one component>`. Resolve them to `<wc_root>/.svn/tmp/<name>`, with the component validated by `RelPath` (no `/`, `..` or `.svn`) and `.svn/tmp` checked with `symlink_metadata` to be a real directory. Reject every other `.svn/...` path.
- Use the admin resolver for the `file-install` source, `file-remove`, `file-move` src/dst and `file-translate` src/dst.
- Add the 5 captured blobs as golden fixtures. Regenerate them with the read-only-directory trick in a `generate.py`, and run the revert pair end to end.

## IMPORTANT

### I1. `file-install` must translate with **pristine** props; `file-translate` with **actual** props
svn: `run_file_install` takes `props` from `svn_wc__db_read_node_install_info`, which returns `pristine_props` (NODES top layer, column 14, `wc_db.c`). eol, keywords, special, executable and needs-lock all come from those props. `run_file_copy_translated` uses `svn_wc__get_translate_info(..., props = NULL)`, which reads ACTUAL props. The PR uses one `install_info(relpath).props`, documented as "Current properties (working props)", for both. With a local uncommitted `svn:eol-style`/`svn:keywords` change, `svn up` or revert writes different bytes than OpenSVN would, and status then disagrees.
Fix: split the trait, e.g. `install_info()` returning pristine props (doc: "NODES top-layer `properties`, as `read_node_install_info`") and `actual_props(relpath)` for `file-translate`. Add a test that sets different prop sets on the two.

### I2. The `file-move` fallback copies through symlinks on any error
`move_file` falls back to `fs::copy(src, dst)` for **any** rename error when `src` exists, not just a cross-device move. `fs::copy` opens `dst` with O_TRUNC and **follows a symlink at `dst`**, so it can write outside the WC (for example, a repo-created `svn:special` symlink at the destination combined with EACCES/EISDIR on rename). It also follows a symlink at `src`. svn's `svn_io_file_move` only copies on EXDEV.
Fix: fall back only on `ErrorKind::CrossesDevices` (EXDEV). Copy into a temp file in dst's directory and `rename_replacing` it over dst (never write through dst), and refuse a symlink `src` (lstat). For `file-translate`, open `src` without following a final symlink (lstat check first) or treat it as special, as `svn_subst_copy_and_translate4` does via `svn_io_check_special_path`.

## MODERATE
- **M1. Special files are read whole into memory.** In `file_install`/`file_translate` (`read_to_end`), a hostile repo can set `svn:special` on a multi-GB text and cause an OOM. svn reads only the first line (`svn_stream_readline`) and streams the rest for unknown types. Read at most the first line (cap it, e.g. 64 KiB, since PATH_MAX-sized targets are enough) and stream the remainder into the regular-file fallback.
- **M2. Windows remove has no retry; read-only handling is swallowed.** `remove_file_if_present` does `let _ = set_read_write` and a single `remove_file`. svn's `svn_io_remove_file2` uses `WIN32_RETRY_LOOP` for sharing violations (antivirus). Recursive `dir-remove` relies on `std::fs::remove_dir_all` honouring read-only files on Windows (needs-lock files), which is untested. Reuse the `rename_replacing` retry policy (a small `remove_file_retrying` contract helper) and add `#[cfg(windows)]` tests: read-only file remove, read-only file inside a recursive dir-remove, and `open_for_times` (FILE_WRITE_ATTRIBUTES, 0x100) on a read-only file. The code reads correctly, but none of it has been verified.
- **M3. Idempotency contract is not written down for `WorkingCopyOps`.** svn requires every item to be re-runnable (`SVN_RUN_WORK_QUEUE_TWICE`). The filesystem items here are idempotent (checked: install, remove, ENOENT move, dir-remove, dir-install, record-fileinfo, sync-flags). The DB-side `file_commit`, `base_remove`, `write_prop_reject_file` and `post_upgrade` must be too, but the trait doc does not say so. Add it, plus a runner test that runs every item twice (as svn's debug flag does).
- **M4. Strict max arity diverges from svn.** svn dispatches on the first atom and ignores extra arguments; the PR rejects them as `Malformed` and stops. That is safe because it stops rather than corrupts, but it is a needless stop for, e.g., legacy `file-install`/`file-remove` variants. Accept `>= min` and ignore the tail, or keep strictness and document it.
- **M5. `tmp-set-text-conflict-markers` / `tmp-set-property-conflict-marker`.** 1.15.0 still dispatches these legacy items, while the PR stops with `UnknownItem`. That is acceptable for format 31+ queues, but document it as an intentional divergence (the message tells the user to run `svn cleanup`).
- **M6. `NodeInstallInfo::is_added` doc says "plain addition".** svn's read-only rule uses `status == added`, which includes copies and moves (moved-here). Fix the doc so the `WcDb` implementer does not exclude copies.
- **M7. Conflict golden coverage is thin.** There are 3 real blobs: update text+prop, update tree edited/deleted, merge text. Missing real-svn cases: a `moved-away` tree conflict with `move_src`/`move_dst` (note that svn's writer puts dst at index 4 when src is absent, and its reader is positional too; the PR mirrors the reader, which is correct), `switch`, a prop conflict with an absent side (atom `""`, e.g. prop added on both sides), and multiple prop names. Add them to `conflicts/generate.py`.
- **M8. `rename_replacing` / io errors are flattened** in a few places (`WorkQueueError::io` via `to_string`), losing `ErrorKind` (DiskFull, PermissionDenied). This is the same residual as #32.

## Conflict skel vs `conflicts.c` (verified)
| Aspect | Result |
|---|---|
| `(WHY CONFLICTS)`, `(op (left right))`, location `("subversion" root uuid-or-() relpath rev kind)` | matches `conflict__prepend_location` / `set_op_*` |
| text `("text" (orig mine their))`, `()` for absent marker | matches `add_text_conflict` |
| prop `("prop" (markers) (names) old mine their)`, `""` when absent | matches `add_prop_conflict` |
| tree `("tree" () reason action [src] [dst])`, token maps | matches `reason_map`/`action_map` and the positional reader |
| newest-first, one per kind | matches (`add` inserts at 0 and refuses duplicates, like the `SVN_ERR_ASSERT`) |
| `is_complete` | equivalent to `svn_wc__conflict_skel_is_complete` |
| `resolve` | equivalent to `svn_wc__conflict_skel_resolve`: removes all text/tree entries, removes the first matching prop name or all for `""`, drops a prop conflict with no names left, keeps unknown kinds, returns `!complete`. Minor: svn errors with `SVN_ERR_INCOMPLETE_DATA` and the PR uses `ErrorCode::Internal` |
| Real blobs decode → encode byte-identical | yes (3 fixtures, incl. explicit-length uuid atom) |

## Runner semantics vs `svn_wc__wq_run` (verified)
- Completion of item N plus its fileinfo is committed atomically when fetching N+1 (`next_item(completed, fileinfo)`), equivalent to 1.15's `wq_record_and_fetch_next`.
- A failed item stays queued (`ItemFailed`, code 155009). Unknown and malformed items also stop (155009), and nothing behind them runs (test `unknown_item_stops_the_run_and_nothing_after_it_runs`).
- Cancellation is polled after the fetch and before dispatch, as in svn.
- `file-install`: keyword URL = root + `svn_path_uri_encode(relpath)`. The PR's char set `!$&'()*+,-./:=@_~` equals `svn_uri__char_validity` (extracted from path.c). rev `-1` when unknown; mtime only if `use_commit_times && changed_date != 0`; read-only iff needs-lock && !lock && !added. All match, apart from the prop source (I1).
- Path safety: WC paths use `RelPath` + `safe_join` (traversal, absolute, drive and symlinked ancestors are rejected, with tests); the final-component symlink is replaced, not followed, by `rename_replacing`. Gaps: C1 (admin paths) and I2 (move fallback).

## Recommended actions
1. C1: admin-tmp resolver + real-blob fixtures + an end-to-end interrupted-revert test.
2. I1: split pristine vs actual props in `WorkingCopyOps`.
3. I2: move fallback only on EXDEV, via temp + rename; no symlink following on src.
4. M1–M3 (special read cap, Windows remove retry + Windows tests, idempotency contract + run-twice test), then M4–M8.

## Unresolved questions
1. Should the admin-tmp resolver live in svn-wc (my preference: it is wc-format knowledge) or be a `svn_types::safe_join_admin_tmp` contract helper?
2. The lead should confirm that refusing `tmp-set-*` legacy items (M5) is acceptable, given that `svn upgrade` refuses non-empty queues. I believe it does, but did not verify it in `upgrade.c`.

Status: DONE
Summary: REQUEST CHANGES. Six real svn 1.15.0 queue blobs round-trip byte for byte, and the run loop and conflict skels match workqueue.c and conflicts.c. But the runner rejects svn's own `.svn/tmp` paths, so an interrupted svn revert or merge can never finish (reproduced). `file-install` uses working instead of pristine props, and the `file-move` fallback can write through a symlink.

---

## Re-review (2026-10-13, head `4e7970c`: fix `9fb2640` + merge of main)

Verdict: **APPROVE** (Windows-only paths remain unverified until Windows CI or a manual run is available).

| Finding | Status | Evidence |
|---|---|---|
| C1 `.svn/tmp` operands | Fixed. `workqueue/paths.rs::resolve_admin_tmp` accepts only `.svn/tmp/<one component>`, checks the component with `RelPath::from_server`, and lstat-checks that `.svn` and `.svn/tmp` are real dirs. It is used only for the install source, remove, move and translate operands; everything else uses `safe_join`. Real recorded queues are in `tests/compat/scenarios/wc/workqueue/queue.txt` | **My repro re-run:** the interrupted-revert pair `(file-install b.txt 1 0 1 1 19 .svn/tmp/svn-bCQBU2)` + `(file-remove 19 .svn/tmp/...)` gives `Ok(2)`; b.txt now holds the reverted content and the tmp file is gone |
| Hostile resolver inputs | All 21 rejected | `.svn/tmp/../x`, `.svn/tmp/../../etc/passwd`, `.svn/tmp/a/b`, `.svn/tmp/`, `.svn/tmp`, `.svn/tmp/.`, `.svn/tmp/..`, `.svn/tmp/.svn`, `a\..\..\x`, `CON`, `a:b`, NUL, `//x`, `.SVN/tmp/x`, `.svn/TMP/x`, `.svn/pristine/..`, `.svn/wc.db`, `/etc/passwd`, `../x`, trailing space, trailing dot. **Symlinked `.svn/tmp`** is rejected ("not a real directory"), and the runner's `file-remove` through it fails with the outside file untouched |
| I1 props split | Fixed: `install_info` = pristine props (doc cites `read_node_install_info`), `actual_props` for `file-translate`; test `file_install_uses_pristine_props_and_file_translate_uses_actual_props` | read + test |
| I2 move fallback | Fixed: plain `rename`; ENOENT with src missing means done; copy only on `CrossesDevices`, via temp in dst dir + `rename_replacing`; non-regular src refused | read (`copy_then_remove`) |
| M1 special streaming | Fixed: `special::install_from_reader` buffers only the first line (≤ 64 KiB) and streams the rest | read |
| M2 Windows remove | Retry loop + read-only clear added; marked "Unverified on Windows" | read only |
| M3 idempotency contract | Documented on `WorkQueueStore`, `WorkingCopyOps` and the module doc | read |
| M4 arity | Now `>= min`, extra args ignored like svn | read |
| M5 `tmp-set-*` | Documented as an intentional divergence | read |
| M6 `is_added` doc | Fixed | read |
| M7 conflict fixtures | Added `update2_mv_txt` (moved-away + src/dst roots), `switch_s_txt`, `update2_p_txt` (3 props, `added` missing on the old side); round-trip tests | read fixtures |

Gates (`CARGO_BUILD_JOBS=2 OPENSVN_REQUIRE_SVN=1`): fmt OK; clippy `--workspace --all-targets --all-features -D warnings` OK; `cargo test -p svn-wc` 96 unit + schema/skel/golden suites OK; `cargo deny` OK; no unwrap/expect in lib.

Residual (non-blocking):
- **A symlink as the final component of a source operand is read through.** In a scratch test, `.svn/tmp/svn-l -> <outside>/secret` used as a `file-install` source copied `SECRET` into `b.txt`. svn behaves the same (`svn_stream_open_readonly` follows links), and only a local actor can plant links in `.svn/tmp`, so this is parity, not a regression. Cheap hardening: lstat the non-special source and refuse symlinks.
- `update2_p_txt` covers a prop missing on one side *inside* a proplist. A whole side being absent (atom `""`) is still only covered by the hand-built test.

Status: DONE
Summary: Every #49 finding is fixed. The interrupted-revert repro now runs end to end, and all hostile `.svn/tmp` inputs, including a symlinked tmp dir, are rejected. APPROVE; the Windows-only paths are still unverified.

### Re-review addendum (found while reviewing #52): 1.14.5 queues name `.svn/pristine/...` operands
Severity: **IMPORTANT**, as a required follow-up. It does not corrupt anything: the runner stops safely with `UnsafePath` and the user can run `svn cleanup`.
Evidence (source at tag 1.14.5): in `update_editor.c` (L3884, L3925) `merge_file` passes `svn_wc__db_pristine_get_path(...)` (the old and new pristine files, **inside** `.svn/pristine/xx/<sha1>.svn-base`) as `merge_left`/`merge_right` to `svn_wc__internal_merge`. `preserve_pre_merge_files` (identical in 1.14.5 and 1.15.0) uses any path inside the wcroot **directly**: `tmp_left = left_abspath`, then `svn_wc__wq_build_file_copy_translated(..., tmp_left, left_copy)`. So an interrupted 1.14.5 (or TortoiseSVN 1.14) update with a text conflict leaves `(file-translate f .svn/pristine/ab/<sha1>.svn-base f.r1)`, which #49's resolver rejects. 1.15.0 sets the textbase aside into `.svn/tmp` first (`svn_wc__textbase_setaside_wq`), which is why the 1.15-recorded queues pass.
Fix: accept, **for the `file-translate` src operand only (read-only)**, exactly `.svn/pristine/<2 hex>/<40 hex>.svn-base` with the 2-hex prefix equal to the first two digits, lowercase. Apply the same real-directory lstat checks, and refuse a symlinked final component. Add a hand-built 1.14.5-shape fixture, since no 1.14.5 binary is available locally.
Verdict stays **APPROVE**, with this follow-up tracked before 11U runs queues from 1.14 working copies.
