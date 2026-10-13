# Bước 0 — track A verification (2026-10-13)

Supplements `from-researcher-to-planner-libsvn-wc-db-format-report.md` (not edited).
Sources: raw files at tags **1.14.5** and **1.15.0** of apache/subversion (`libsvn_subr/skel.c`,
`libsvn_wc/{wc-metadata.sql,wc-queries.sql,wc_db.c,wc_db_wcroot.c,wc_db_util.c,wc.h,upgrade.c}`),
plus empirical checks with local svn 1.15.0 and svn 1.14.5 (Docker `alpine:3.22`, SQLite 3.49.2).
Tests that lock these facts in: `crates/svn-wc/tests/{skel_real_blobs,schema_structure,schema_open}.rs`.

## 1. skel.c — canonical grammar (VERIFIED, file identical in 1.14.5 and 1.15.0)
- Byte classes (locale independent table `skel_char_type`): space = `\t \n \f \r ' '` (NOT `\v`);
  digit = `0-9`; name-start = ASCII letters only; "paren" class = `( ) [ ]`; everything else
  (incl. `_ - : . /` and bytes >= 0x80) is class nothing.
- Parse: `(` starts a list; a letter starts an *implicit* atom that runs until whitespace, a paren-class byte or end of
  data; anything else must be an *explicit* atom `<digits><one space-class byte><len bytes>`.
  Inside a list: any amount of whitespace between items; `)` ends the list. Leading zeros in the length are accepted.
  Length overflow (value > remaining input) and a missing/short payload fail. Empty input is invalid.
- `svn_skel__parse` consumes only a prefix and ignores trailing bytes (callers never check). OpenSVN:
  `Skel::parse` = strict (trailing non-whitespace is an error), `Skel::parse_prefix` = svn behaviour.
- Unparse (canonical writer): an atom is implicit iff `0 < len < 100`, the first byte is a letter and no later byte is
  whitespace/paren-class; otherwise `"<len> <bytes>"` (so `0 ` for the empty atom, `100 ...` for 100+ bytes). Lists:
  `(` + children joined by ONE space + `)`. No trailing newline.
- Proplist skel = even-length flat list `(name value name value ...)`, all atoms; order is APR hash order
  (non-deterministic), so readers must not depend on order; OpenSVN writes caller order (callers sort for determinism).
  iprops skel = `(path (proplist) path (proplist) ...)`.
- Empirical: every real blob extracted from svn 1.15.0 wc.db files (NODES/ACTUAL_NODE properties, inherited_props,
  27 distinct WORK_QUEUE items, 3 conflict_data skels incl. text + prop + tree conflicts) round-trips byte-for-byte through
  parse + unparse. Observed work item opcodes: `file-install`, `file-remove`, `file-translate`, `sync-file-flags`,
  `prej-install`, `dir-install`, `file-commit` (arity/args are track B's M6 scope).
- Technique used to capture WORK_QUEUE items (they vanish once the queue has run): add a trigger
  `AFTER INSERT ON work_queue` that copies `NEW.work` into a scratch table inside the test wc.db; svn does not mind extra objects.

## 2. wc-metadata.sql — format 31 schema (VERIFIED)
- `STMT_CREATE_SCHEMA` (1.14.5) and the creation schema in 1.15.0 are identical modulo comments
  (checked by stripping comments/whitespace): REPOSITORY, WCROOT, PRISTINE, ACTUAL_NODE, LOCK, WORK_QUEUE, WC_LOCK, NODES,
  EXTERNALS; indexes I_UUID, I_ROOT, I_LOCAL_ABSPATH, I_PRISTINE_MD5, I_ACTUAL_PARENT, I_NODES_PARENT, I_NODES_MOVED,
  I_EXTERNALS_DEFINED; views NODES_CURRENT, NODES_BASE; triggers nodes_{insert,delete,update_checksum}_trigger.
  In 1.14.5 `user_version` comes from `-- define: SVN_WC__VERSION` (31); in 1.15.0 from `SVN_WC__SUPPORTED_VERSION` (31).
- `STMT_INSTALL_SCHEMA_STATISTICS` (`ANALYZE sqlite_master` + sqlite_stat1 rows for NODES, ACTUAL_NODE, LOCK, WC_LOCK,
  EXTERNALS + final `ANALYZE sqlite_master`) is identical in both tags.
- 1.15.0 adds `STMT_UPGRADE_TO_32` (PRISTINE.hydrated, I_PRISTINE_UNREFERENCED, TEXTBASE_REFS, SETTINGS, new bodies for
  nodes_delete_trigger / nodes_update_checksum_trigger). The format 33 block is commented out and "format 99" is dead text:
  both ignored, as planned.
- Gotcha: the bundled SQLite (rusqlite) is compiled with STAT4, so `ANALYZE` creates `sqlite_stat4`, which svn's SQLite usually
  lacks. OpenSVN drops it right after installing the hints so `sqlite_master` matches (checked against both svn versions).
- Gotcha: trigger/view text in `sqlite_master` keeps comments from the source SQL (svn strips them when it embeds the SQL), so the
  structural test compares bodies after comment/whitespace normalisation; tables are compared via PRAGMA introspection.
- Empirical result: structure (tables, columns, types, not-null, defaults, pk, FKs, index columns/uniqueness, triggers, views,
  `sqlite_stat1` rows, `user_version`=31, `application_id`=0, `page_size`=4096, `auto_vacuum`=0) of an OpenSVN-created wc.db
  equals a wc.db from an svn 1.15.0 checkout AND from an svn 1.14.5 checkout.

## 3. Versions and open path (wc.h, wc_db_wcroot.c) (VERIFIED)
- 1.14.5: `SVN_WC__VERSION 31` (newest supported), so a 1.14.5 client REJECTS format 32 (`SVN_ERR_WC_UNSUPPORTED_FORMAT`,
  "client too old"). 1.15.0: `SVN_WC__VERSION 32`, `SVN_WC__SUPPORTED_VERSION 31` (minimum), `SVN_WC__DEFAULT_VERSION 31`.
  Consequence: never writing 32 keeps WCs usable by both; 32 appears only via `--compatible-version=1.15` (checkout/upgrade),
  verified: `svn checkout --compatible-version=1.15` yields user_version 32 and the SETTINGS table.
- `svn_wc__db_pdh_create_wcroot` policy: `format < 4` too old (check out again); `format > SVN_WC__VERSION` unsupported
  ("client too old"); `format < SUPPORTED_VERSION` (with verify) gives `SVN_ERR_WC_UPGRADE_REQUIRED` (run `svn upgrade`), and for
  format >= 13 with an unfinished work queue the upgrade is refused (cleanup first with an old client).
  OpenSVN mapping: every refusal is `ErrorCode::WcFormatUnsupported` with distinct messages ("upgrade ... svn upgrade" for <31,
  "newer than supported" for >32). Probing is read-only, so a rejected WC is byte-identical afterwards (tested).
- Open sequence: `fetch_sdb_info` in one lock: WCROOT id (`SELECT id FROM wcroot WHERE local_abspath IS NULL`, missing row means
  corrupt), `user_version`, `verify_stats_table` (only when format == 31 == `SVN_WC__ENSURE_STAT1_TABLE`: if `sqlite_stat1`
  is absent re-run the statistics SQL, errors ignored), `read_settings` (format >= 32: `SELECT store_pristine FROM settings WHERE wc_id`, else true).
- wcroot discovery: nearest ancestor containing `.svn/wc.db`; a `.svn` with only a legacy `entries` file (format < 12) is a pre-1.7 WC.
  `.svn/format` and `.svn/entries` both contain `12\n` in wc-ng WCs (svn 1.15 output, checked); pristine subfolders are created on demand.
- Creation (`svn_wc__db_init` -> `init_db`): CREATE_SCHEMA, statistics, REPOSITORY row, `INSERT INTO wcroot (local_abspath) VALUES (NULL)`,
  root BASE row `(wc_id, '', 0, NULL, repos_id, repos_path, revision, presence, depth, 'dir')` with presence `incomplete` when revision > 0
  else `normal`. OpenSVN `WcDb::create` does the same, and svn 1.15.0 then runs add/commit/update/cleanup on it without complaint (tested).

## 4. wc-queries.sql — NODES_CURRENT semantics (VERIFIED)
- `NODES_CURRENT` = rows of NODES whose `op_depth` equals `MAX(op_depth)` for the same `(wc_id, local_relpath)`;
  `NODES_BASE` = `op_depth = 0`. Both are plain views (~19-20 uses in each tag, same shape). Some hot queries avoid the view with a
  subquery to prevent a table scan (e.g. `STMT_INSERT_DELETE_LIST`); A2 should keep that pattern for recursive ops.
- Queries rely on macros that our SQL layer must expand: `IS_STRICT_DESCENDANT_OF(...)`, `MAP_*` presence tokens, `RELPATH_SKIP_JOIN`,
  and the SQLite function `relpath_depth(local_relpath)` registered in `svn_wc__db_util_open_db` (deterministic, TEXT to depth, '' = 0).
  Needed in A2 (requires the rusqlite `functions` feature).
- `STMT_SELECT_NODE_INFO` returns all layers `ORDER BY op_depth DESC`; "current" is the first row.

## 5. sqlite settings cross-check (wc_db_util.c / wc-queries.sql)
- Only `STMT_PRAGMA_LOCKING_MODE` (`locking_mode = exclusive` + `journal_mode = DELETE`) differs, and it runs ONLY when the user config
  `[working-copy] exclusive-locking` is set (the `exclusive` flag of `svn_wc__db_util_open_db`); default is off. OpenSVN never enables it
  (it would block TortoiseSVN). Journal `TRUNCATE`, busy timeout 10 s, locking NORMAL as in the lead's settings note. Decision:
  `synchronous=FULL` on OpenSVN's own write connection (note item 2); read-only connections skip the journal/synchronous pragmas.
- Empirical: after OpenSVN writes through `WcDb::with_write` the file header bytes 18/19 stay `1,1` (rollback journal) and `svn status/cleanup` stay clean.

## 6. Impact on later milestones
- A2: add the `functions` feature to rusqlite for `relpath_depth`; port the macros; keep `IS_STRICT_DESCENDANT_OF` semantics (case-sensitive).
- B (M4): PRISTINE refcount triggers exist in format 31 as described; format 32 changes both trigger bodies (TEXTBASE_REFS), so read-only paths must not rely on trigger text.
- B (M6/M7): use `svn_wc::skel::Skel`; observed work item opcodes are listed in section 1.

## Unresolved
- subst.c, full workqueue item arity, conflicts.c grammar: track B scope (not verified here).
- svn registers a custom NFD-aware `like()` function with `case_sensitive_like`; not replicated yet. Revisit with the Vietnamese-name APFS test (A2/B3).

## A2 findings (nodes, wc_lock) — verified against svn 1.15.0 behaviour
- Working-layer row shapes (checked row-for-row by `tests/nodes_ops_vs_svn.rs`): `op_depth` of an add/delete/copy = number of segments of the
  operation root; added rows get `changed_date = 0`; delete inserts `base-deleted` rows for the node and every present descendant at
  `op_depth = depth(root)` and removes higher layers below it; deleting inside a copied tree layers over the copy rows; replace = same-`op_depth`
  `INSERT OR REPLACE`; copy copies pristine props to NODES and local edits to the copy's ACTUAL; move = copy with `moved_here=1` + delete layer with
  `moved_to` on the root only; revert removes whole layers rooted at the node and keeps changelist membership; delete clears local props/conflicts but keeps changelist.
- Commit: BASE row gets `revision=changed_revision=new_rev`; copy children keep their own `changed_*`; committed delete leaves a `not-present` BASE row with `changed_date=0`;
  changelists are dropped unless `--keep-changelists`; svn also fills `translated_size`/`last_mod_time` from the working file (caller does it via `record_fileinfo`).
- WC_LOCK: `locked_levels` L on dir D covers D and descendants up to L levels below (-1 = all); obtain checks node exists (except root), empty work queue (normal mode),
  foreign rows below (ignoring those deeper than `levels`), and the nearest row at/above that reaches the dir. An svn commit holds the root lock for its whole duration
  (observed with a sleeping pre-commit hook); `svn commit` refuses to run while our row exists (E155004).
- Ownership is outside wc.db; risk noted in `wc_lock.rs` docs: a dead record plus a later svn lock on the same directory is indistinguishable from the stale row.
## Corrections after review of the schema PR
- `reverse_unordered_selects` is NOT a normal svn setting: it is compiled in only for SVN_DEBUG builds (sqlite.c). OpenSVN enables it under `debug_assertions` only (section 5 above and the lead's settings note list it as unconditional; treat that row as debug-only).
- `open` probes with a READ_WRITE connection (falls back to read-only for write-protected files), so a hot journal left by a crashed writer is rolled back like svn does; the `sqlite_stat1` re-install runs in one `BEGIN IMMEDIATE` with a 100 ms busy timeout and is skipped (debug log) when the database is busy.

## Review follow-ups (nodes, wc_lock)
- Nodes: copy now carries the source's delete layers (shifted to the destination depth) and refuses locally added, incomplete, not-present, excluded and mixed-revision sources with `WcFormatUnsupported`
  (A3: full mixed-revision and non-present support, which needs more svn oracle cases); recursive revert removes working layers of the whole subtree; non-recursive revert of an operation root with
  child operations returns svn's E155038 (`svn_code = 155038`); committing a replacement drops the old BASE subtree; the batch API `commit_nodes` runs a whole commit in one transaction (parents first);
  moving a local copy is a plain copy (no `moved_here`); `decode_props` reads a zero-length blob as no properties. Observed in svn: delete layers carried by a copy have `changed_date = 0`.
- wc_lock: row first, then ownership record, then marker file `.svn/tmp/opensvn-lock-*` (skel `(pid boot dir levels)`); cleanup removes a row only for a dead owner's record plus a matching marker;
  no marker means foreign (`needs_user_action`). The orphan-row sweep is gone. Verified with real svn: `svn cleanup` empties `.svn/tmp`, so after crash + cleanup + a new svn lock our recovery leaves svn's row alone.
  Open question for the integration phase: confirm on Windows that TortoiseSVN "Break write locks" also clears `.svn/tmp`.
- Windows: an SQLite connection in DELETE journal mode cannot delete the journal our TRUNCATE-mode connection keeps open (`SQLITE_IOERR_DELETE`); the library maps IOERR_DELETE/IOERR_LOCK to `WcBusy`.
