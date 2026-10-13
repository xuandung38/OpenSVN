# Bước 0 (track B) — verification report: subst.c, translate.c, workqueue.c, conflicts.c

Author: dev-07b. Date: 2026-10-13. Supersedes the UNVERIFIED items 4, 5, 7 of `from-researcher-to-planner-libsvn-wc-db-format-report.md` for track B.
Sources read at tags **1.14.5** and **1.15.0** (`apache/subversion`, raw files; copies kept outside the repo in `~/Documents/06-Code/svnsrc/`).

## 0. 1.14.5 vs 1.15.0 diff (track B files)
| File | Result |
|---|---|
| `libsvn_wc/translate.c` | **identical** |
| `libsvn_subr/subst.c` | translation logic identical. Only: `svn_subst_translation_required` now uses `apr_hash_count(keywords) > 0` (empty hash no longer forces translation); `svn_subst_stream_translated` returns the original stream when no eol/keywords (optimisation); `char *keyword` constness. No byte-level behaviour change. |
| `libsvn_wc/workqueue.c` | **all `svn_wc__wq_build_*` builders byte-identical.** 1.15 changes the *runner*: `file-install` uses `svn_wc__working_file_writer_*` and `record-fileinfo` is batched (`wq_record_fileinfo` + `svn_wc__db_wq_record_and_fetch_next`). Same on-disk items. |
| `libsvn_wc/conflicts.c` | comment typos + one guard (`store_pristine` in a deprecated API). Skel grammar identical. |

Consequence: one implementation serves both; golden tests recorded with svn 1.15.0 are valid for 1.14.5 semantics.

## 1. Translation (M5)
### 1.1 Entry rules (`translate.c`)
- Props used: `svn:eol-style`, `svn:keywords` (empty value == absent), `svn:special` (presence only), `svn:executable`, `svn:needs-lock` (presence only).
- `svn_subst_eol_style_from_value`: `native`->eol `APR_EOL_STR` style native; `LF`/`CR`/`CRLF`->fixed; absent->none; **any other (case-sensitive) -> unknown, eol NULL**.
- `translation_required(force_eol_check=TRUE)` as used by libsvn_wc: special || keywords non-empty || style != none.
- **To working (expand, `FROM_NF`)**: eol = style eol (native -> platform), **repair = TRUE always**, keywords built with real values. Unknown style: keywords only, eol NULL.
- **To normal form (`TO_NF`)**: native -> eol `"\n"`, repair = `FORCE_EOL_REPAIR` flag only (default FALSE => mixed EOLs raise `SVN_ERR_IO_INCONSISTENT_EOL`, E135000); fixed -> eol stays the style's eol (**repo stores CRLF/CR files with CRLF/CR**; only native is LF in the repo), repair forced TRUE; unknown -> `SVN_ERR_IO_UNKNOWN_EOL`; keywords contracted using a key-only table built with url "", rev "-1", date 0, author "" (`for_normalization`).
- Verified by experiment (svn 1.15.0): `propset` itself rejects mixed files for every style; to reach commit-time repair, set the prop first and edit the file afterwards. Result: LF/CR/CRLF styles silently repair to the style's eol at commit; native fails with E200009 "inconsistent newlines". Fixtures `eol_*_mixed_after_prop`.
- Special: `svn_subst_read_specialfile`: symlink -> text `link <target>` (no newline), regular file -> raw bytes. Install (`create_special_file_from_stream`): first line (up to `\n`), prefix `link ` -> symlink to the rest of the line; any other content -> regular file with the bytes unchanged; symlink unsupported platform -> regular file with the internal representation. Atomic via unique temp + rename.
- Flags (`io_set_perms`, `svn_wc__sync_flags_with_props`), Unix: execute: set -> `x |= r>>2` per class; clear -> remove x bits of classes with r bit. Read-only: clear w for classes with r bit. Read-write: **owner rw only** (`|= 0600`). Symlinks untouched. Sync rule: read-write unless (status normal && working props have needs-lock && no lock); in that case read-only **only if committed props also have needs-lock**. Windows: only the read-only attribute; executable ignored.

### 1.2 Keyword algorithm (`subst.c`)
- Tokens of `svn:keywords` split on `" \t\v\n\b\r\f"` (note `\b`). `name=fmt` => custom keyword (build_keywords3), value = `keyword_printf(fmt)`; custom entries may shadow built-ins.
- Built-in groups (expansion value; all spellings enabled together):
  - `LastChangedRevision`|`Revision` (exact) or `Rev` (**case-insensitive**) -> keys LastChangedRevision, Revision, Rev = `%r`
  - `LastChangedDate` (exact)|`Date` (ci) -> keys LastChangedDate, Date = `%D`
  - `LastChangedBy`|`Author` -> `%a`; `HeadURL`|`URL` -> `%u`
  - `Id` (ci) -> `%b %r %d %a`; `Header` (ci) -> `%u %r %d %a`
  - Text matching in the file is **case-sensitive** against these canonical key spellings (`$id$`, `$REV$` untouched).
- `keyword_printf` codes: `%a` author, `%b` basename of URL (**percent-decoded**, empty for root URL), `%d` UTC `YYYY-MM-DD HH:MM:SSZ`, `%D` `svn_time_to_human_cstring` (**local time zone**, `YYYY-MM-DD HH:MM:SS +hhmm (Www, DD Mon YYYY)`, hours = `gmtoff/3600` truncating with forced sign so -00:30 prints `+00`, English names in C locale), `%P` repos-relative path (percent-decoded; needs root and url), `%R` root, `%r` revision, `%u` URL as given, `%_` space, `%%` `%`, `%H` = `%P %r %d %a`, `%I` = `%b %r %d %a`; unknown `%x` printed literally; trailing lone `%` printed. Date 0/NULL -> empty.
- Scanner (`translate_chunk`): candidate = `$` ... next `$`, max `SVN_KEYWORD_MAX_LEN`=255 bytes, aborted by CR/LF. Name = bytes up to first `:` (or end). Match requires name in table. Forms handled by `translate_keyword_subst`: unexpanded `$K$`, expanded `$K: v $` / `$K:$`, **fixed-length** `$K:: v   $` (needs `6+len(K) < len`; pad with spaces, truncate with `#` before closing `$`; contract = blank fill). Expanded value is truncated to `255-5-len(K)`. A matched-but-malformed candidate keeps buffering and rolls back to the *last* `$` seen on newline/overflow (`next_sign_off`).
- The fixed-length fill (`while (*p != '$')`) stops at the FIRST `$` after the value, which can be an interior `$` when the scanner kept buffering past a failed candidate (`$Rev:: a$b $` expands to `$Rev:: 1$b $`). Ported and golden-tested.
- Known svn quirk, reproduced and golden-tested: `$Rev:: $ $Rev::      $` — the short first candidate is not a valid fixed-length keyword, so the scan extends across the second `$` and the pair is treated as one fixed-length candidate. Expand+contract is therefore **not** always an identity; fixture `kw_fixed_too_short_lossy` compares only forward.
- EOL scanner: CR, LF, CRLF recognised; lone CR is an EOL. First EOL seen defines the source format; later different EOL -> error unless repair. Output always the configured eol. Nothing special at EOF (pending CR flushed as CR; open keyword buffer flushed raw, **without** rollback).

### 1.3 `$Date$`: time zone and locale (decisions)
- `%D` uses the offset in effect **at the commit timestamp** (DST included). `Keywords::build` takes a resolver `&dyn Fn(i64 /*unix secs*/) -> i32 /*utc offset secs*/` supplied by svn-client; svn-wc has no tz dependency. Fixtures cover UTC, +0700, -0230 (DST), -0700.
- **Intentional difference from svn:** the suffix `(Www, DD Mon YYYY)` is always rendered in the English C locale. svn gettext-translates it and uses locale names (`LC_ALL=de_DE.UTF-8` gives `(Di., 13. Okt. 2026)`), so real svn output varies per machine. The text is cosmetic: contraction discards the value, so status, commit content and checksums never depend on it.
- Pre-1970 dates: APR truncates toward zero, this port floors. Not ported (low impact).

### 1.4 Chunking (byte parity, pinned to svn 1.15.0)
svn's `next_sign_off` rollback marker is local to each `translate_chunk` call, so malformed candidates straddling a chunk boundary behave differently. Measured with svn 1.15.0:
- normalise (commit, status comparison): **16384-byte** chunks (`SVN__STREAM_CHUNK_SIZE`). `"x"*16377 + "$Rev:a$Id: x $\n"` commits unchanged.
- expand (checkout/install): **102400-byte** chunks (`SVN_STREAM_CHUNK_SIZE`); boundary straddling cases at 16384/32768/65536 expand normally, at 102400 they do not.
`ContentTranslator` exposes `chunk_size()`; the slice helpers and `translate_stream` use these sizes. svn 1.14.5 was not run (only 1.15.0 is installed); its source is identical for this logic, so the sizes are assumed equal.

### 1.5 `svn:special`
svn branches on `special` before any eol/keyword handling. `to_working`/`to_repo_normal` return special content unchanged; use `special::*`. svn also treats a path that is a symlink on disk as special without the property (`svn_io_check_special_path`); the status/work-queue callers must do the same.

## 2. Work queue (M6) — exact builders (identical in 1.14.5/1.15.0)
Skel = `(op arg...)`; strings are atoms, ints are decimal atoms; all paths are **wc-root-relative relpaths** (resolved against the wc root of `wri_abspath`). The queue holds a list of items; `wq_merge` flattens. Dispatch table (runner must know all, else `SVN_ERR_WC_BAD_ADM_LOG` "Unrecognized work item" and stop):
| op | builder skel | notes |
|---|---|---|
| `file-install` | `(file-install relpath use_commit_times record_fileinfo [source_relpath])` | source omitted => pristine of `checksum` from NODES (`read_node_install_info`); props/changed_date from DB; temp file in wcroot tmp, translate (repair=TRUE), `final_mtime=changed_date` if `use_commit_times`; read-only if needs-lock && !lock && status != added; executable per prop; install via rename; `record_fileinfo` => record (mtime,size) into NODES after the item. |
| `file-remove` | `(file-remove relpath)` | ignore ENOENT |
| `file-move` | `(file-move src_relpath dst_relpath)` | `svn_io_file_move` (cross-device ok); ENOENT on src == already done |
| `file-translate` (copy_translated) | `(file-translate local_relpath src_relpath dst_relpath)` | arg1 supplies props; copy with expand, repair=TRUE |
| `sync-file-flags` | `(sync-file-flags relpath)` | = `svn_wc__sync_flags_with_props` |
| `prej-install` | `(prej-install relpath)` (pre-1.9: `+ conflict skel`) | reads prop conflict from DB, writes `.prej` via temp + rename |
| `dir-install` | `(dir-install relpath)` | mkdir (+ add to wc) |
| `dir-remove` | `(dir-remove relpath [1])` | trailing `1` => recursive |
| `file-commit` | `(file-commit relpath)` | post-commit: install `process_commit_file_install` (extra legacy args ignored) |
| `postupgrade` | `(postupgrade)` | upgrade only |
| `base-remove` (legacy) | `(base-remove relpath not_present_rev keep_flag?)` | old: `(base-remove relpath keep_not_present)`; new: 2 ints |
| `record-fileinfo` (legacy) | `(record-fileinfo relpath [mtime])` | optional mtime set (non-special files), record stat |
| `tmp-set-text-conflict-markers`, `tmp-set-property-conflict-marker` (legacy) | not persisted by modern clients | accept-or-error |
Run semantics: loop fetch next by id -> dispatch -> mark done (record map flushed with the next fetch in 1.15). Failure wraps `SVN_ERR_WC_BAD_ADM_LOG` and leaves the item.

## 3. Conflict skel (M7) — `conflicts.c`
```
conflict   := (WHY CONFLICTS)
WHY        := ()                       ; operation not set yet
            | (OP (ORIGIN-LEFT ORIGIN-RIGHT))
OP         := "update" | "switch" | "merge" | "patch"      ; patch not written by update/switch/merge
ORIGIN     := () | ("subversion" repos_root_url (uuid | ()) repos_relpath peg_rev kind)   ; kind = "file"|"dir"|"none"|"unknown"
CONFLICTS  := (CONFLICT ...)           ; empty => not complete
CONFLICT   := text | prop | tree
text       := ("text" (original_marker mine_marker their_marker))      ; each marker = relpath atom or ()   [builder prepends their, mine, original]
prop       := ("prop" (marker_relpath ...) (conflicted_prop_name ...) old_props mine_props their_props)
              ; each *_props = props-skel list or empty atom "" when absent
tree       := ("tree" () reason action [move_src_op_root_relpath [move_dst_op_root_relpath]])
              ; reason/action are tokens (edited, deleted, added, replaced, moved-away, moved-here, missing, obstructed, unversioned / edit, delete, add, replace); marker list always empty
```
Order is positional (new conflicts are prepended to CONFLICTS; kinds looked up by first atom). `update`/`switch` origins = (original, target); `merge` = (left, right). Complete == WHY set and CONFLICTS non-empty. Exact token tables (`reason_map`, `action_map`) to be copied from `conflicts.c` L~460-520 during B2 and pinned by a skel round-trip test against wc.db values produced by svn 1.14.5/1.15.0.

## 4. Remaining UNVERIFIED (not needed for B1)
- Pristine install rename order (`wc_db_pristine.c`), `svn_sqlite.c` busy/journal -> owned by 07A/B1-part2.
- `svn_skel__parse_int` / `unparse` byte grammar (explicit vs implicit-length atoms): 07A skel.rs; B2 relies on it.
- Tree conflict `reason_map`/`action_map` token lists (above).

## 5. Follow-ups
1. Time-zone resolver implementation lives in svn-client (P11).
2. Golden fixtures were recorded on macOS (native EOL = LF). `native` with CRLF is tested through `to_working_with_native`; a real Windows byte comparison is covered by P06/P13 on the Windows CI runner.
3. The EPERM `reown_file` retry of `io_set_perms` is not ported.
