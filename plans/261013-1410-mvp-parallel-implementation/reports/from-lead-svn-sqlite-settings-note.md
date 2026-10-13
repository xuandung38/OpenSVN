# wc.db SQLite settings used by Subversion (gate for svn-wc and app read path)

Date: 2026-10-13. Sources: `subversion/libsvn_subr/sqlite.c` at tags 1.14.5 and 1.15.0
(settings block is identical in both; only the minimum SQLite version differs:
3.8.2 in 1.14.5, 3.24.0 in 1.15.0), plus an empirical check on a fresh working copy
made by `svn` 1.15.0 (`svnadmin create` + `svn checkout file://...`).

## Settings svn applies on every connection (`svn_sqlite__open`)

| Setting | Value | Source |
|---|---|---|
| `journal_mode` | `TRUNCATE` (never WAL) | `PRAGMA journal_mode = TRUNCATE;` ("faster than DELETE on Windows") |
| `synchronous` | `OFF` | `PRAGMA synchronous=OFF;` (atomic on app crash, not on power loss) |
| `locking_mode` | `NORMAL` | `PRAGMA locking_mode = NORMAL;` (no exclusive locking) |
| busy timeout | 10000 ms (`BUSY_TIMEOUT`) | `sqlite3_busy_timeout(db, 10000)` when no explicit timeout is given |
| `foreign_keys` | `OFF` (ON only in SVN_DEBUG builds) | |
| `recursive_triggers` | `ON` | needed so triggers fire on INSERT OR REPLACE |
| `case_sensitive_like` | `1` | plus custom `like()` function (NFD-aware) when built with normalization fixes |
| `temp_store` | `MEMORY` | best effort |
| `reverse_unordered_selects` | `ON` | surfaces missing ORDER BY |
| open flags | `READONLY` / `READWRITE` / `CREATE`, `NOMUTEX` | |

## Empirical check (svn 1.15.0, fresh checkout)
- Files in `.svn/`: `wc.db`, `wc.db-journal` (rollback journal, persisted file), `format`, `entries`, `pristine/`, `tmp/`.
- A new `sqlite3` connection reports `journal_mode=delete`, `locking_mode=normal`, `synchronous=2`, `busy_timeout=0`,
  `page_size=4096`, `user_version=31`, `application_id=0`, `foreign_keys=0`.
  These are SQLite defaults because the pragmas above are per-connection (TRUNCATE is not persisted in the file); it confirms the file is NOT in WAL mode.
- `WCROOT` has one row `(1, NULL)`; `WC_LOCK` is empty when idle.

## Decisions for OpenSVN
1. wc.db: open with the same settings: `journal_mode=TRUNCATE`, `locking_mode=NORMAL`, `busy_timeout=10000`. Never WAL
   (a WAL file would break concurrent svn/TortoiseSVN, which expect a rollback journal).
2. `synchronous`: svn uses OFF. OpenSVN should use NORMAL or FULL for its own writes (crash safety goal) since pristine and
   work-queue ordering must survive power loss; this is compatible with svn readers. Decide in svn-wc; do not copy OFF blindly.
3. Keep write transactions short (no lock held across network I/O): other svn processes wait up to 10 s then fail with SQLITE_BUSY.
4. Read path (status in the app): plain readers under rollback journal block while a writer commits; use busy timeout and
   retry, and never hold a read transaction open between calls.
5. WC_LOCK has no PID column (see wc-db-format report section 6); ownership lives outside wc.db (`LockOwnershipStore`).
6. Use `rusqlite` with bundled SQLite >= 3.24 (svn 1.15 minimum).
