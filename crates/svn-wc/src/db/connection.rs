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

/// Maps a rusqlite error onto the engine error model. Lock contention that survives the busy
/// timeout becomes [`ErrorCode::WcBusy`].
pub fn sql_err(err: rusqlite::Error) -> SvnError {
    use rusqlite::ErrorCode as Sqlite;
    let code = match err.sqlite_error_code() {
        Some(Sqlite::DatabaseBusy | Sqlite::DatabaseLocked) => ErrorCode::WcBusy,
        Some(Sqlite::DatabaseCorrupt | Sqlite::NotADatabase) => ErrorCode::Corrupt,
        Some(Sqlite::DiskFull) => ErrorCode::DiskFull,
        Some(Sqlite::PermissionDenied | Sqlite::ReadOnly) => ErrorCode::PermissionDenied,
        _ => ErrorCode::Internal,
    };
    SvnError::new(code, format!("wc.db: {err}"))
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
         PRAGMA temp_store = MEMORY;
         PRAGMA reverse_unordered_selects = ON;",
    )
    .map_err(sql_err)
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
