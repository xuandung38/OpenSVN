//! SQLite connection setup and error mapping for wc.db.
//!
//! Settings mirror `svn_sqlite__open` (libsvn_subr/sqlite.c, identical in 1.14.5 and 1.15.0) as
//! recorded in the lead's sqlite settings note. wc.db stays in a rollback-journal mode
//! (`TRUNCATE`, never WAL) so svn and TortoiseSVN keep sharing the file; the journal mode is a
//! per-connection setting and is never persisted into the file by us.

use std::path::Path;
use std::time::Duration;

use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use svn_types::{ErrorCode, Result, SvnError};

/// Busy timeout applied by svn when no explicit timeout is given (`BUSY_TIMEOUT` in sqlite.c).
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(10_000);

/// How a connection is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Existing database, read only (used for format 32 and for readers).
    ReadOnly,
    /// Existing database, read and write.
    ReadWrite,
    /// Create a new database file (fails later if it already holds a schema).
    Create,
}

/// `SQLITE_IOERR_DELETE` (`SQLITE_IOERR | 10 << 8`).
const SQLITE_IOERR_DELETE: std::ffi::c_int = 2570;
/// `SQLITE_IOERR_LOCK` (`SQLITE_IOERR | 15 << 8`).
const SQLITE_IOERR_LOCK: std::ffi::c_int = 3850;

/// Maps a rusqlite error onto the engine error model. Lock contention that survives the busy
/// timeout becomes [`ErrorCode::WcBusy`].
pub fn sql_err(err: rusqlite::Error) -> SvnError {
    use rusqlite::ErrorCode as Sqlite;
    // A sharing violation while another process holds the journal or lock file (typically a
    // Windows peer that does not use svn's TRUNCATE journal mode) is contention, not a failure.
    let sharing_violation = matches!(
        &err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == SQLITE_IOERR_DELETE || e.extended_code == SQLITE_IOERR_LOCK
    );
    let code = match err.sqlite_error_code() {
        _ if sharing_violation => ErrorCode::WcBusy,
        Some(Sqlite::DatabaseBusy | Sqlite::DatabaseLocked) => ErrorCode::WcBusy,
        Some(Sqlite::DatabaseCorrupt | Sqlite::NotADatabase) => ErrorCode::Corrupt,
        Some(Sqlite::DiskFull) => ErrorCode::DiskFull,
        Some(Sqlite::PermissionDenied | Sqlite::ReadOnly) => ErrorCode::PermissionDenied,
        _ => ErrorCode::Internal,
    };
    SvnError::new(code, format!("wc.db: {err}"))
}

/// Opens an existing database for probing and normal use: read/write when the file allows it
/// (so a hot journal left by a crashed writer is rolled back exactly like svn does), otherwise
/// read-only. Returns the connection and whether it can write.
pub fn open_existing(path: &Path) -> Result<(Connection, bool)> {
    match open_connection(path, OpenMode::ReadWrite) {
        // SQLite silently downgrades a READWRITE open of a write-protected file to read-only.
        Ok(conn) => {
            let writable = !conn.is_readonly(rusqlite::MAIN_DB).map_err(sql_err)?;
            Ok((conn, writable))
        }
        Err(e) if e.code == ErrorCode::PermissionDenied => {
            open_connection(path, OpenMode::ReadOnly).map(|c| (c, false))
        }
        Err(e) => Err(e),
    }
}

/// Opens `path` and applies the svn-compatible per-connection settings.
pub fn open_connection(path: &Path, mode: OpenMode) -> Result<Connection> {
    let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX;
    flags |= match mode {
        OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        OpenMode::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
        OpenMode::Create => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    };
    let conn = Connection::open_with_flags(path, flags).map_err(sql_err)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(sql_err)?;
    apply_pragmas(&conn, mode != OpenMode::ReadOnly)?;
    register_functions(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection, writable: bool) -> Result<()> {
    if writable {
        let mode: String = conn
            .query_row("PRAGMA journal_mode = TRUNCATE", [], |r| r.get(0))
            .map_err(sql_err)?;
        if !mode.eq_ignore_ascii_case("truncate") {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("wc.db: could not select rollback journal mode TRUNCATE (got {mode})"),
            ));
        }
        // svn uses OFF; OpenSVN keeps pristine/work-queue ordering durable across power loss.
        conn.execute_batch("PRAGMA synchronous = FULL;")
            .map_err(sql_err)?;
    }
    conn.execute_batch(
        "PRAGMA locking_mode = NORMAL;
         PRAGMA foreign_keys = OFF;
         PRAGMA recursive_triggers = ON;
         PRAGMA case_sensitive_like = 1;
         PRAGMA temp_store = MEMORY;",
    )
    .map_err(sql_err)?;
    // svn enables this only in SVN_DEBUG builds (it surfaces missing ORDER BY clauses); mirror that.
    if cfg!(debug_assertions) {
        conn.execute_batch("PRAGMA reverse_unordered_selects = ON;")
            .map_err(sql_err)?;
    }
    Ok(())
}

/// Registers `relpath_depth(local_relpath)` exactly like `svn_wc__db_util_open_db`: the number of
/// path segments (`''` is 0), NULL for anything that is not text.
fn register_functions(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "relpath_depth",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            Ok(match ctx.get_raw(0) {
                ValueRef::Text(t) => {
                    let slashes = t.iter().filter(|b| **b == b'/').count();
                    Some(i64::from(!t.is_empty()) + i64::try_from(slashes).unwrap_or(i64::MAX))
                }
                _ => None,
            })
        },
    )
    .map_err(sql_err)
}
