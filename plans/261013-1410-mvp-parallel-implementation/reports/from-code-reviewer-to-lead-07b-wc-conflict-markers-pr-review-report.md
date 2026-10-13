# PR #52 review — 07B conflict marker files

Reviewer: code-reviewer. Date: 2026-10-13. Head `a92bec6`; own diff only (`review/pr49c..a92bec6` = commit `897ee94`): `conflicts/markers.rs`, `conflicts/markers/tests.rs`, `conflicts.rs` → `conflicts/mod.rs` (+2 lines, `pub mod markers;`).
Upstream compared at 1.15.0: `libsvn_wc/merge.c` (`preserve_pre_merge_files`, `detranslate_wc_file`), `update_editor.c` (labels), `libsvn_client/merge.c` (labels), `conflicts.c` (`svn_wc__conflict_create_markers`), `io.c` (`svn_io_open_uniquely_named`). Behaviour checked against svn 1.15.0 on this machine.

## Verdict: REQUEST CHANGES (1 IMPORTANT; everything else is minor)

Naming, the unique-name loop and work-item order are byte-identical to svn in every case I reproduced. The one blocker is an API footgun that can queue deletion of the user's working file.

## Gates (`CARGO_BUILD_JOBS=2 OPENSVN_REQUIRE_SVN=1`, macOS)
| Gate | Result |
|---|---|
| fmt | OK |
| clippy `--workspace --all-targets --all-features -D warnings` | OK |
| `cargo test -p svn-wc --locked` | OK (105 unit + suites) |
| `cargo deny check` | OK |
| unwrap/expect/unsafe in lib | none |
| Owned paths | only `crates/svn-wc/src/conflicts/**` |

## Parity evidence (svn 1.15.0 vs PR)
Scenario: pre-create `t.txt.mine`, `t.txt.r2`, `t.txt.2.r2`, plus a **dangling symlink** `t.txt.r1 -> /nonexistent` and `d/dir_conflicts.prej`, then run `svn up` to get a text conflict on `t.txt` and a property conflict on `d`.

| | svn 1.15.0 (wc.db `conflict_data` / `ls`) | PR (scratch test calling `create_text_markers` / `create_prop_marker`) |
|---|---|---|
| original | `t.txt.2.r1` (dangling symlink skipped, not followed) | `t.txt.2.r1` |
| mine | `t.txt.2.mine` | `t.txt.2.mine` |
| their | `t.txt.3.r2` | `t.txt.3.r2` |
| dir prop | `d/dir_conflicts.2.prej` | `d/dir_conflicts.2.prej` |
| merge + `preserved-conflict-file-exts=txt` | `m.txt.merge-left.r1.txt`, `m.txt.merge-right.r2.txt`, `m.txt.working.txt` | same (`for_merge` + `with_ext`) |
| update + ext | `m.txt.r1.txt`, `m.txt.r2.txt`, `m.txt.mine.txt` | same |
| switch (fixture from #49) | `s.txt.2.r10` for the colliding `their` name | same loop: `<name>.<n><suffix>`, n from 2 |

Glob (`svn_cstring_match_glob_list` → `apr_fnmatch`, flags 0), compared against svn's actual output for ext `txt`: `txt`, `t?t`, `*`, `[st]xt`, `[!a]xt`, `[^a]xt` match, and `T*` does not (case-sensitive). All of these match svn, **except the backslash escape `tx\t`**: svn matches it, the PR does not (M1).

Work-item order: `file-translate` ×3 (left, right, target), then `file-remove <detranslated>`, then the removals of staged temps. This equals `preserve_pre_merge_files`: copy L, copy R, copy T, remove detranslated, then `last_items`. Example output: `(file-translate t.txt 10 .svn/tmp/l t.txt.2.r1) (file-translate t.txt 10 .svn/tmp/r t.txt.3.r2) (file-translate t.txt 10 .svn/tmp/t t.txt.2.mine) (file-remove 10 .svn/tmp/t)`. The prop marker is prepended to the markers list and `(prej-install <local>)` is emitted, as `svn_wc__conflict_create_markers` does.

Creation safety: `OpenOptions::create_new` (O_CREAT|O_EXCL) never follows or reuses an existing file or (dangling) symlink, which I verified. The receiving directory is `safe_join`-checked plus lstat-checked for a symlinked last component. On Windows, PermissionDenied on an existing name continues the loop like svn's EACCES-on-directory case (unverified on Windows). External sources are staged into `.svn/tmp/marker-*` (names accepted by #49's resolver) and removed by trailing items.

## IMPORTANT

### I1. `detranslated_target: MarkerSource::InWc(rel)` queues `file-remove rel` with no check, so a caller can delete the working file
In svn the detranslated copy is **always** a fresh temp in `.svn/tmp`: `detranslate_wc_file(..., force_copy = TRUE)` takes the `svn_io_open_unique_file3` branch, so removing it is safe. The PR accepts any WC relpath. In a scratch test, passing `InWc("t.txt")`, the natural mistake of handing in "the working file", returns:
```
(file-translate t.txt t.txt t.txt.mine) (file-remove t.txt)
```
The queue then deletes the user's working file. Its content survives only in `.mine` until a later install rewrites the file.
Fix: require the detranslated target to be an admin temp. Either take an `AdminTmp` newtype or a `PathBuf` that the function stages itself, or validate `workqueue::paths::is_admin_tmp(rel)` and return `UnsafePath` otherwise. Add a test that `InWc("t.txt")` is refused. Also document that `InWc` sources for original/their must be paths the runner accepts: `.svn/tmp/<name>` or WC paths, **not** `.svn/pristine/...` (see the #49 addendum).

## MODERATE
- **M1. The glob has no backslash escape.** `apr_fnmatch` without `APR_FNM_NOESCAPE` treats `\x` as a literal `x`. The PR compares `\` literally, so `tx\t` matches in svn and not in the PR (verified against svn). Add escape handling in `glob_match`, both outside and inside `[...]`. This is rare in configs, but it is a cheap fix for parity.
- **M2. Partial failure leaves empty marker files.** If the 2nd/3rd `create_unique_file` or a staging copy fails, the earlier markers stay as empty orphans. svn has the same behaviour, so this is acceptable, but document it, or remove the created markers on error (best effort) since nothing references them yet.
- **M3. Order when the detranslated target is `External`.** Its staged removal is appended after the original/their staged removals instead of right after the three copies. svn can never produce this case, because the detranslated copy is always in `.svn/tmp`. With I1 fixed (admin temp only) the variant disappears; otherwise move it to the `remove_target` position.
- **M4. Localized svn labels.** `libsvn_client/merge.c` wraps the labels in `_()`. For example, `ja.po` translates `.merge-left.r%ld%s%s` to `.merge-left.r%ld`, dropping the preserved extension. OpenSVN uses the C-locale names, which is the right fixed choice; note it as an intentional divergence next to the `$Date$` locale note from #30.

## Recommended actions
1. I1: restrict the detranslated target to an admin temp, plus a refusal test.
2. M1: glob escape; M2/M3 docs or cleanup.
3. Follow-up from the #49 addendum (`.svn/pristine/...` `file-translate` src from 1.14.5 queues) touches the same `InWc` contract; decide both together.

## Unresolved questions
1. Should `create_text_markers` own detranslation (stage the working file itself into `.svn/tmp`), removing the caller footgun entirely? Reviewer preference: yes, once `detranslate_wc_file` cases I–IV land.

Status: DONE
Summary: REQUEST CHANGES. Marker naming (collisions, dangling symlink, `dir_conflicts.N.prej`, preserved extensions) and work-item order match svn 1.15.0 byte for byte. One IMPORTANT issue: `InWc` detranslated targets are unchecked, so a caller can queue `file-remove` of the user's working file. The glob is also missing backslash escapes.

---

## Re-review (2026-10-13, head `f83df82`: fixes `4b6f107`, `f83df82`)

Verdict: **APPROVE** (the Windows-only paths remain unverified; there is no Windows CI).

Note on scope: the PR base now shows `feat/07b-wc-workqueue-conflicts`. The fix also touches `workqueue/{paths,runner,tests}.rs` (owned by 07B) to close the #49 addendum. That is the agreed single admin-path contract, so it is in scope.

| Check | Result | Evidence (scratch tests at `f83df82`, not committed) |
|---|---|---|
| `AdmPath` accepts exactly 2 shapes | **Pass** | `resolve_adm_path`: `.svn/tmp/<one>` (component checked with `RelPath::from_server`) or `.svn/pristine/<xx>/<40 lowercase hex>.svn-base` with `xx == sha[0..2]`; anything else gets "only ... are accepted". Each directory on the way is lstat-checked as a real dir, and a symlinked final component is refused (this also closes my #49 residual) |
| 21 hostile inputs | **All rejected**, with `allow_pristine` both false and true | the same list as the #49 re-review |
| Pristine variants | **All rejected** | uppercase dir+hash, uppercase hash only, dir `cd` ≠ `ab`, `.svn-base` with no hash, no `.svn-base` suffix, 39-hex hash, trailing `/x`, extra dir level, `../pristine/...` traversal |
| Symlinks | **Rejected** | symlinked `.svn/pristine/ab` ("not a real directory"), symlinked pristine file ("final component is a symlink"), symlinked `.svn/pristine`, symlinked `.svn/tmp/<name>` |
| Pristine only as a source | **Pass** | Runner rejects `file-remove <pristine>`, `file-move <pristine> a`, `file-move a <pristine>`, `file-translate a a <pristine>` ("may only be used as a read-only source"), the `file-install`/`sync-file-flags` node relpath (admin dir), and the pristine file is untouched each time. `(file-translate a.txt <pristine> a.txt.r1)` runs and copies the content (the 1.14.5 queue shape) |
| Detranslation owned by `create_text_markers` | **Pass** | The function now takes `working_props: &TranslateInfo`; `stage_detranslated_working_file` writes the repo-normal form (repair = TRUE, like `detranslate_wc_file(force_copy=TRUE)`; a symlink becomes `link <target>`) into `.svn/tmp/marker-*`. The detranslated-target `InWc` parameter is gone. `MarkerSource::InWc` remains **only for original/their sources**, is validated up front through `resolve_operand(.., allow_pristine = true)`, and is never removed |
| No `file-remove` of a WC path | **Pass** | `queueing_can_never_remove_the_working_file_or_any_wc_path` iterates the source combinations (InWc `t.txt` ×2, tmp + External, External ×2) and asserts every removal starts with `.svn/tmp/marker-` and that the working file is unchanged. It is an enumerated test, not a `proptest`, but it covers the input classes |
| Glob escapes | **Pass** | `tx\t` ✓, `t[\x]t` ✓, `\*` does not match `txt`, `t[]x]t` ✓, `t[a-z]t` ✓, unclosed `[` is literal; the earlier svn-verified set still matches (`txt`, `t?t`, `*`, `T*`=no, `[st]xt`, `[!a]xt`, `[^a]xt`) |
| External delete order | **Pass, matches svn** | `translate L, translate R, translate T, remove T(detranslated), remove L(staged), remove R(staged)`, i.e. `preserve_pre_merge_files` (copy L/R/T, remove detranslated, then `last_items` L, R) |

Gates (`CARGO_BUILD_JOBS=2 OPENSVN_REQUIRE_SVN=1`): fmt OK; clippy `--workspace --all-targets --all-features -D warnings` OK; `cargo test -p svn-wc` 116 unit + 18 + 9 integration OK (the svn-backed suites ran because svn 1.15.0 is required); `cargo deny` OK; no unwrap/expect/unsafe in `markers.rs` or `paths.rs`.

Residual (non-blocking):
- `.svn/pristine/...` is also accepted as a `file-install` source. svn never writes that, but it is read-only and harmless; keep it or narrow it to `file-translate`.
- M2 (orphan empty markers after a partial failure) and M4 (localized labels) are unchanged; please document them.

Status: DONE
Summary: All four checks pass. `AdmPath` accepts exactly the two shapes; all 21 hostile inputs and 9 pristine variants are rejected, along with every symlink case; a pristine works only as a read-only source. Detranslation is staged internally, so no WC path can be queued for removal. Glob escapes and the External delete order match svn. APPROVE.
