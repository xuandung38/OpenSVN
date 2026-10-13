# libsvn_wc WC-DB format 31/32: spec for pure-Rust compatible engine

Date: 2026-10-13 | Author: researcher | Consumer: planner
Context: ADR-004 (`docs/adr/adr-004-working-copy-format.md`), options B = compat with format 31 + 32.

## Source basis and caveats
- Primary source = Subversion **trunk** mirror, fetched 2026-10-13:
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/wc-metadata.sql (schema + upgrade stmts)
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/wc.h (version constants, format history)
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/wc_db.c
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/wc_db_pristine.c
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/workqueue.c
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/conflicts.c
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/adm_files.c
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/translate.c
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/include/private/svn_wc_private.h
  - https://raw.githubusercontent.com/apache/subversion/trunk/subversion/libsvn_wc/wc-queries.sql (fetched, only skimmed)
- **Not verified**: 1.14.x and 1.15.x release tags (trunk used as proxy), `notes/wc-ng/*`, `svn_sqlite.c`, `skel.c`, `subst.c`, `wc-queries.sql` in full, release notes page. Items marked (UNVERIFIED) need a check.
- Internal ADR claims (E155021 error on old clients, ~50% size saving) are NOT verified against source.
- Only ~12 web/network calls used; budget was tight.

## 1. Schema (format 32 final state; format 31 = same minus hydrated/TEXTBASE_REFS/SETTINGS)

`PRAGMA user_version` = format number. Read via `svn_sqlite__read_schema_version` (wc_db.c ~L16190). Client max = `SVN_WC__VERSION 32`, min = `SVN_WC__SUPPORTED_VERSION 31`, default-created = `SVN_WC__DEFAULT_VERSION 31` (wc.h).

- **REPOSITORY** (id PK autoinc, root TEXT UNIQUE = URI-encoded root URL, uuid). One repo may map to several roots (http vs https).
- **WCROOT** (id PK, local_abspath UNIQUE; NULL = metadata stored in the wcroot itself). Each row = one WC root (wc_id). Multi-WC DB possible (nested/switched roots share the DB at wcroot).
- **PRISTINE** (checksum TEXT PK = **SHA-1 hex**, compression INT (always NULL today), size INT NOT NULL = byte size of file, refcount INT NOT NULL, md5_checksum TEXT NOT NULL (non-unique index I_PRISTINE_MD5), **hydrated INT NOT NULL DEFAULT 1 (fmt 32 only)**).
  - fmt 32 adds index `I_PRISTINE_UNREFERENCED (refcount, refcount=0)` for GC.
- **NODES** (PK wc_id, local_relpath, op_depth; unique I_NODES_PARENT(wc_id,parent_relpath,local_relpath,op_depth); unique I_NODES_MOVED(wc_id,moved_to,op_depth)):
  - `op_depth`: 0 = BASE (checkout tree). >0 = WORKING layer; value = depth (path segments) of the operation root. All nodes in one op share op_depth.
  - `parent_relpath`: "" for wcroot; NULL only for wcroot BASE node.
  - `repos_id`, `repos_path`, `revision`: BASE = repo location; WORKING = copy/move source (set on op root and all children).
  - `presence`: BASE: normal | not-present | incomplete | server-excluded (was "absent" pre-fmt30, renamed by upgrade) | excluded. WORKING: normal (added/copied), not-present (copy of a not-present base), incomplete, base-deleted (deletion layer; WORKING only), excluded.
  - `kind`: file|dir|symlink|unknown (unknown when not present).
  - `moved_here` INT (set on every node of moved destination tree); `moved_to` TEXT (set only on root of source move; NULL for children; target always in highest op_depth layer at destination).
  - `properties` BLOB skel (NULL = no info for non-normal presence; empty skel/NULL = no props).
  - `depth` TEXT (NULL = infinity). `checksum` TEXT REFERENCES PRISTINE (SHA-1; text base for file).
  - `symlink_target` TEXT (kind=symlink).
  - `changed_revision`, `changed_date` (APR usec since 1970), `changed_author`.
  - **Cache columns**: `translated_size` INT (size of working file when unmodified, after EOL/keyword translation), `last_mod_time` INT (APR usec mtime when last known unmodified). Both NULL for non-files or not computed.
  - `dav_cache` BLOB skel (props from server). `file_external` INT (non-NULL = file external). `inherited_props` BLOB skel (fmt 31; non-NULL only on BASE of WC root / update-switch roots).
- **Views**: `NODES_CURRENT` = row with MAX(op_depth) per (wc_id, local_relpath). `NODES_BASE` = op_depth=0. Almost every query uses NODES_CURRENT semantics; implement the layered lookup, not a flat table.
- **Triggers on NODES** (refcount maintenance): AFTER INSERT when checksum NOT NULL -> pristine.refcount+1; AFTER DELETE when OLD.checksum NOT NULL -> refcount-1; AFTER UPDATE OF checksum when changed -> +1 new, -1 old. **fmt 32** replaces delete/update triggers to also `DELETE FROM TEXTBASE_REFS` for the same (wc_id, local_relpath, op_depth).
- **ACTUAL_NODE** (PK wc_id, local_relpath; unique I_ACTUAL_PARENT(wc_id,parent_relpath,local_relpath)): `properties` BLOB (local prop changes; NULL = none), `changelist` TEXT, `text_mod` TEXT ("unknown"/"admin"/"noticed"), `conflict_data` BLOB skel (fmt 30+ current conflict store), `older_checksum`/`left_checksum`/`right_checksum` REFERENCES PRISTINE (NOT counted by triggers; code must maintain refcount manually or skip). Legacy columns `conflict_old`, `conflict_new`, `conflict_working`, `prop_reject`, `tree_conflict_data` are cleared by the fmt-30 upgrade; treat as read-only legacy (UNVERIFIED whether 1.15 still reads them).
  - Row may exist with only changelist/conflict data on a non-present node.
- **LOCK** (PK repos_id, repos_relpath; lock_token, lock_owner, lock_comment, lock_date INT usec). Cache only, not authoritative.
- **WORK_QUEUE** (id PK autoinc, work BLOB NOT NULL = serialized skel). See section 4.
- **WC_LOCK** (PK wc_id, local_dir_relpath; locked_levels INT NOT NULL DEFAULT -1; -1 = infinite depth). See section 6.
- **EXTERNALS** (PK wc_id, local_relpath; parent_relpath NOT NULL (write-only per TODO), repos_id, presence (MAP_NORMAL|MAP_EXCLUDED), kind, def_local_relpath, def_repos_relpath, def_operational_revision, def_revision (both nullable)). Unique I_EXTERNALS_DEFINED(wc_id, def_local_relpath, local_relpath).
- **TEXTBASE_REFS** (fmt 32): PK (wc_id, local_relpath, op_depth). Row = this NODES row's text base must stay on disk (pinned); absence = text base may be dehydrated.
- **SETTINGS** (fmt 32): PK wc_id -> WCROOT.id, `store_pristine` INT. Migrated on upgrade with `INSERT OR IGNORE ... SELECT id, 1 FROM WCROOT`.
- Index stats: `sqlite_stat1` rows are inserted at create time (wc-metadata.sql ~L602-629). Not required for compatibility but affects query plans; reproduce them.
- Dump-compat test: run `sqlite3 .svn/wc.db .schema` on svn-created WC and diff against engine output.

## 2. Format 31 vs 32 and compatibility rules

Exact diff (`STMT_UPGRADE_TO_32`, wc-metadata.sql):
1. `ALTER TABLE PRISTINE ADD COLUMN hydrated INTEGER NOT NULL DEFAULT 1`
2. `CREATE INDEX I_PRISTINE_UNREFERENCED ON PRISTINE (refcount, refcount=0)`
3. `CREATE TABLE TEXTBASE_REFS (wc_id, local_relpath, op_depth, PK(all three))`
4. Replace `nodes_delete_trigger` and `nodes_update_checksum_trigger` (add textbase_refs cleanup).
5. `CREATE TABLE SETTINGS (wc_id PK, store_pristine)` + migrate existing WCs to store_pristine=1.
6. `PRAGMA user_version = 32`.
No other schema change in 32. Format 33 block is commented out in trunk (inactive).

Format history (wc.h): 31 added `inherited_props` (+ index rebuilds) — shipped in 1.8–1.14 as format 31. 1.15 shipped format 32 + multi-wc-format support.

Read/write rules (from wc.h + wc_db.c + adm_files.c):
- Client opens any format in [31, 32]. Format < 31 -> upgrade path (`svn_wc__upgrade_sdb`); format > 32 -> `SVN_ERR_WC_UNSUPPORTED_FORMAT` (rejects; 1.14 client on fmt 32 = same class of error; ADR "E155021" (UNVERIFIED code string)).
- Upgrades from 31 -> 32 happen automatically in 1.15 clients; old 1.8–1.14 clients will NOT open a 32 WC. **Downgrade is not supported by svn.** Engine must never silently bump 31 -> 32 unless user asked for store-pristine=no.
- Creating a WC: `target_format` must be in [SUPPORTED, VERSION]. If `store_pristine=FALSE` and `target_format < SVN_WC__HAS_OPTIONAL_PRISTINE (32)`, svn errors (adm_files.c ~L290). So store-pristine=no implies format 32.
- Store-pristine mismatch between requested op and stored SETTINGS is an error (adm_files.c ~L317-327), so engine must read SETTINGS and match.
- Format 32 WC with store_pristine=yes behaves like 31 + extra tables; pristine rows are hydrated=1.

## 3. Pristine store

- Path: `<wcroot>/.svn/pristine/<first 2 hex of sha1>/<sha1-hex>.svn-base` (wc_db_pristine.c `get_pristine_fname`, `PRISTINE_STORAGE_EXT ".svn-base"`).
- Content: raw bytes, **no compression** (PRISTINE.compression always NULL; column reserved).
- Key = SHA-1; MD5 kept for legacy repo protocol only (`md5_checksum`, non-unique).
- `size` in PRISTINE = file size; used to verify file "proper" before use.
- Temp: `.svn/tmp` (`SVN_WC__ADM_TMP`, adm_files.c) and wcroot `tmp` (WCROOT_TEMPDIR_RELPATH, wc_db.c L67). Install = write temp file, verify checksum, rename into pristine dir (wc_db_pristine.c install path; exact rename order UNVERIFIED vs `notes/wc-ng/pristine-store`).
- Refcount: maintained by NODES triggers (section 1). Unreferenced (refcount=0) files are removed by pristine cleanup, not immediately. Note: ACTUAL_NODE conflict checksums are not in triggers.
- Hydration (fmt 32): `hydrated=0` = file removed from disk; row kept. Reads of non-hydrated text go through the repo-fetch path (`stmt_select_pristine` returns hydrated flag, wc_db_pristine.c L116-185). A text base referenced by TEXTBASE_REFS must not be dehydrated.
- Pristine files are only needed for: diff/revert/merge base, text-mod check (unless translated_size/last_mod_time cache hits), conflict base.
- Store-pristine=no reads base from repo on demand; svn does NOT keep a copy. Disk saving ~ half (ADR claim, UNVERIFIED).

## 4. Work queue

- Table WORK_QUEUE(id, work). Items are skels: `(OP_NAME arg...)`; args are atoms (strings or decimal ints). Items processed in id order; an item is deleted after success. Interrupted runs resume (crash safety). Items are added in the same SQLite txn as the metadata change that needs them (`svn_wc__db_wq_add`, wc_db.c L13576).
- Op names (workqueue.c L48-64) and what a compatible client MUST execute (for WCs left mid-operation by TortoiseSVN/svn CLI):
  - `file-install` (MUST): (local_relpath use_commit_times record_fileinfo). Writes working file from pristine, applies translation from props (special/executable/needs-lock/eol-style/keywords), sets mtime, records fileinfo. Arity per builder (workqueue.c L670-704) — verify.
  - `file-remove` (MUST): (local_relpath). Remove working file.
  - `file-move` (MUST): rename file, used by moves.
  - `file-translate` / `file-copy-translated` (MUST): copy from pristine into working file with translation.
  - `file-commit` (MUST for svn compat only if we ever commit; engine does not commit in MVP?): post-commit fixups, detranslate. Pure-read clients can ignore by refusing to proceed with WQ unfinished.
  - `sync-file-flags` (MUST): apply svn:executable/needs-lock flags to file.
  - `prej-install` (MUST): install .prej file (prop-reject).
  - `dir-install` (MUST): create directory.
  - `dir-remove` (MUST): remove directory (only if empty of versioned/unversioned children per rules).
  - `base-remove` (MUST): remove BASE rows (delete tree leftovers).
  - `record-fileinfo` (MUST): write translated_size/last_mod_time/text-mod cache after install.
  - `postupgrade` (MUST for 1.7->1.8-style upgrades only; can ignore for 31/32 WCs).
  - `tmp-set-text-conflict-markers`, `tmp-set-property-conflict-marker` (temporary/internal; not expected in persisted queues, UNVERIFIED).
- Skel syntax: canonical libsvn skel = atoms `<len>:<bytes>`, lists `(...)`. Implement exact encoder/decoder (libsvn_subr/skel.c, UNVERIFIED file name/path; check `svn_skel.h` in trunk).
- Run semantics: `svn_wc__wq_run` (workqueue.c L1510). Compatible engine must run queue on open/cleanup, or refuse writes until queue empty (ADR consequence).

## 5. Conflict storage

- Since format 30: conflict info = skel in `ACTUAL_NODE.conflict_data` (BLOB). Legacy text/prop/tree columns cleared on upgrade.
- Skel structure (conflicts.c; derived, dump to verify):
  - Top-level list: first child = operation list: `(update|switch|merge <origins...>)` via `SVN_WC__CONFLICT_OP_UPDATE/SWITCH/MERGE` (conflicts.c L212-309). Origins = list of version locations.
  - Per conflict: kind atom `text` | `prop` | `tree` (`SVN_WC__CONFLICT_KIND_*`), then per-kind data (paths, marker info, reason/action atoms).
  - Version location (conflict__prepend_location, L98-134): `("subversion" repos_root_url repos_uuid repos_relpath rev kind)`; `repos_uuid` may be empty list.
- Text conflict files: `.mine`, `.rN`, `.rM`, `.working` via basenames/relpaths; plus `older/left/right` checksums in ACTUAL_NODE columns referencing PRISTINE.
- Prop conflicts: `.prej` file (prop_reject info) installed by `prej-install` work item; marker files need matching content format (conflict markers via svn_diff output). Byte-compat markers are needed for TortoiseSVN "resolved" logic.
- Tree conflicts: stored only in conflict_data of victim node (ACTUAL_NODE); tree_conflict_data column dead.
- A "format 99" cleanup block that would drop tree_conflict_data is present in trunk file but has no `STMT_` marker and is not executed. Do NOT copy it into schema code.

## 6. WC_LOCK and concurrency with TortoiseSVN

- WC_LOCK rows = directory-level locks: (wc_id, local_dir_relpath, locked_levels). `locked_levels` = depth below that dir that is locked (-1 = infinity). Lock check (wc_db.c ~L14772-14776) uses locked_levels + row_depth >= dir_depth.
- Obtain = INSERT row. Conflict -> `SVN_ERR_WC_LOCKED` unless `steal_lock` (wclock_steal). Ownership (`owned_locks`) is per-process in-memory; cross-process ownership is by DB row only. **No PID/hostname stored** -> stale locks only cleared by `svn cleanup` or steal. (Verify: no PID column in schema — confirmed in wc-metadata.sql L230-238.)
- Engine must: insert WC_LOCK on write op for each dir being modified, release on end (DELETE). Do not leave orphan rows (`STMT_DELETE_WC_LOCK_ORPHAN*`).
- SQLite concurrency: svn relies on SQLite locking (busy handling). Busy timeout value and journal mode UNVERIFIED (check `svn_sqlite.c`). Keep txns short (ADR consequence). Do not hold open write txn across network calls.
- Interaction with TortoiseSVN: both are svn-lib based (TortoiseSVN uses libsvn_wc), so same WC_LOCK protocol applies. If TortoiseSVN has a lock, engine must fail with a clear "WC locked" error rather than steal, unless user consents.

## 7. Translation and status cache

- Translation props read from NODES.properties (or ACTUAL_NODE for local changes): `svn:special`, `svn:executable`, `svn:needs-lock`, `svn:eol-style`, `svn:keywords` (workqueue.c L569-576 names; translate.c = wrapper for subst.c).
- EOL/keyword expansion: repo-normal form (LF in repo, keywords collapsed) <-> working form; `translated_size` = size of working file after translation of pristine (so an unmodified file has stat size == translated_size). Also `last_mod_time` = mtime of file when last determined unmodified.
- Status fast path (ADR "status cache"): if stat(size, mtime) == (translated_size, last_mod_time) -> unmodified, no hashing. Otherwise compare content (translate pristine vs working). Then record-fileinfo updates the cache. Must update these two fields on every install or svn will report wrong status (pitfall).
- svn:special: working file = "link <target>" text for symlinks; svn:executable -> chmod +x on POSIX only.
- Keywords expansion is the riskiest part: exact bytes must match svn (Date/Revision/Author/Id/Header/URL formats, abbreviated keyword forms). Use subst.c as oracle (UNVERIFIED path; in trunk libsvn_subr/subst.c).

## 8. Suggested work-package breakdown (independent modules)

| # | Module | Interface (inputs/outputs) | Depends on |
|---|---|---|---|
| M1 | skel codec | encode/decode canonical skel <-> tree | none |
| M2 | sqlite schema + format detect/upgrade (31 read/write, 32 read/write, SETTINGS) | open(wc_root) -> Db handle; create(format, store_pristine) | M1 |
| M3 | NODES layered model (NODES_CURRENT, op_depth, moved_*) | query current node, list children, write op layer | M2 |
| M4 | pristine store (sha1 path, tmp+rename, refcount/GC, hydration/TEXTBASE_REFS) | install(bytes)->sha1; read(sha1); dehydrate/hydrate | M2 |
| M5 | translation (eol/keywords/special/executable) | translate_to_working(pristine, props) ; detranslate | none (pure) |
| M6 | work-queue runner (all op kinds) | run(db) with per-op handlers | M1,M2,M4,M5 |
| M7 | conflict skel + marker files (.prej, text markers) | read/write conflict_data | M1,M2,M4 |
| M8 | WC_LOCK | obtain/release/steal per dir | M2 |
| M9 | status cache (translated_size/last_mod_time) | check_unmodified(path) | M3,M4,M5 |
| M10 | EXTERNALS/file-external rows, dav_cache, inherited_props | read/write | M2 |

Order: M1+M5 (pure, parallel) -> M2 -> M3,M4,M8 (parallel) -> M6,M7,M9 -> M10. Each module ships with golden-file tests.

Test strategy:
- Oracle: real `svn` binaries. Build Docker images with svn 1.14.x (format 31 only) and 1.15.x (format 31 and 32). Confirm exact versions via `svn --version` in image.
- Scenario matrix: checkout; edit/add/delete/copy/move; update with conflicts; switch; externals; svn:eol-style/keywords/executable/special; sparse (depth=empty/files); store-pristine=no; interrupted op (kill -9 mid-update, then run svn cleanup); concurrent lock (two processes).
- Differential: (a) svn creates WC -> engine reads -> `svn status/info/diff` output equal; (b) engine creates WC -> svn 1.14/1.15 reads -> same status. Compare `sqlite3 .dump` after normalizing ids/timestamps.
- Schema diff: `.schema` and `PRAGMA user_version` on engine-created WC must equal svn's byte-for-byte (modulo whitespace).
- Byte-compat for pristine files: sha1 names match `sha1sum` of file; md5 column matches.
- Corpus: real-world WCs including Windows-created (CRLF, TortoiseSVN) for eol and locked-state tests.

## 9. Pitfalls

1. Do not assume format 31 default = format 32; `svn checkout` creates 31 unless `--store-pristine=no`.
2. Format 32 WCs are invisible to svn 1.8–1.14. Never upgrade implicitly.
3. NODES_CURRENT semantics: forgetting op_depth layering breaks status after copy/move/delete.
4. PRISTINE.refcount is trigger-maintained; manual edits or missing triggers leak files or delete live ones. ACTUAL_NODE checksums are NOT counted.
5. translated_size/last_mod_time wrong -> false "modified" or missed modifications.
6. Work queue left unfinished by another client -> must run it before writes (or fail safely). Do not drop items.
7. Keyword/EOL output bytes must equal svn exactly, including on Windows CRLF repos.
8. Stale WC_LOCK rows: no PID -> need `svn cleanup`-equivalent; do not auto-steal silently.
9. Conflict skel encoding is positional; reordering children breaks TortoiseSVN resolve.
10. Skel atoms are length-prefixed binary-safe: do not use UTF-8 string functions blindly.
11. SQLite in WAL vs rollback journal: TortoiseSVN and svn expect the same journal mode; wrong mode can corrupt concurrent access (UNVERIFIED which mode svn uses).
12. Format 33 is commented out in trunk; do not implement it. Future format bumps need checking against 1.16+ release.
13. Dead `format 99` block in wc-metadata.sql: not executed; do not copy.
14. Byte-level equality for `sqlite_stat1` not required, but query plans depend on it.
15. Mixed-case/Unicode relpaths: svn stores relpaths using '/' normalized and NFC? (UNVERIFIED; check `svn_relpath` / path normalization before assuming.)

## Unresolved questions
1. Exact version tags: confirm trunk schema == 1.15.x tag (wc-metadata.sql, wc.h) and check 1.14.x tag has no format-32 items.
2. Does svn 1.15 still read legacy ACTUAL_NODE conflict_old/new/working/prop_reject columns?
3. Busy timeout value and SQLite journal mode used by libsvn_wc (`svn_sqlite.c`).
4. Exact arity and argument order of each work-queue item (workqueue.c build functions L387-1072 need full read).
5. Conflict skel full grammar (conflicts.c L317+ and tree conflict encoding).
6. Pristine install temp-file rename order and dehydration/rehydration paths (`notes/wc-ng/pristine-store`).
7. Keyword/EOL byte-exact rules (`subst.c`) and CRLF handling for svn:eol-style=native.
8. Whether E155021 is the exact error code old clients emit on fmt 32 (ADR claim).
9. Whether ~50% saving claim (ADR) holds for real repos; measure in pilot.
10. Whether svn normalizes relpath Unicode (NFC/NFD) — affects macOS (HFS+/APFS) WCs.
11. Homebrew/Xcode svn versions on macOS dev machines: which format do they produce by default?

Status: DONE_WITH_CONCERNS
Summary: Schema, format 31->32 diff, pristine layout, work-queue op names, lock table, and conflict skel location verified from libsvn_wc trunk source. Conflict skel grammar, translation byte rules, busy/journal settings and 1.14/1.15 tag equality are not verified.
Concerns: Trunk used as proxy for 1.14/1.15; web budget spent before reading subst.c, svn_sqlite.c, skel.c, and the full workqueue builders. Plan M5 and M7 need a second research pass before implementation.
