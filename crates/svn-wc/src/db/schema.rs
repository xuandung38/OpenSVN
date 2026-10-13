//! wc.db schema creation and format detection (format 31 write, 31/32 read).
//!
//! The schema SQL is taken from `libsvn_wc/wc-metadata.sql` at tag 1.14.5 (Apache License 2.0,
//! see the header of each `.sql` file). Format 32 only differs by the `PRISTINE.hydrated` column,
//! the `TEXTBASE_REFS` and `SETTINGS` tables and two trigger bodies; OpenSVN reads those but
//! never creates them.

use rusqlite::{Connection, OptionalExtension};
use svn_types::{ErrorCode, Result, SvnError};

use super::connection::sql_err;

/// Format created and written by OpenSVN.
pub const FORMAT_WRITE: u32 = 31;
/// Newest format that can be opened (read only).
pub const FORMAT_MAX_READ: u32 = 32;

const CREATE_SCHEMA_SQL: &str = include_str!("sql/create-schema-format-31.sql");
const INSTALL_STATISTICS_SQL: &str = include_str!("sql/install-schema-statistics.sql");

/// Reads `PRAGMA user_version`.
pub fn read_user_version(conn: &Connection) -> Result<u32> {
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql_err)?;
    u32::try_from(v).map_err(|_| SvnError::new(ErrorCode::Corrupt, "wc.db: bad user_version"))
}

/// Validates a format number against what this build supports, with svn-equivalent policy:
/// older than 31 needs `svn upgrade`, newer than 32 needs a newer OpenSVN.
pub fn check_supported_format(format: u32) -> Result<()> {
    if format < FORMAT_WRITE {
        return Err(SvnError::new(
            ErrorCode::WcFormatUnsupported,
            format!(
                "working copy format {format} is too old (minimum {FORMAT_WRITE}); \
                 upgrade it with the svn command line client (`svn upgrade`)"
            ),
        ));
    }
    if format > FORMAT_MAX_READ {
        return Err(SvnError::new(
            ErrorCode::WcFormatUnsupported,
            format!(
                "working copy format {format} is newer than this version supports \
                 (maximum {FORMAT_MAX_READ})"
            ),
        ));
    }
    Ok(())
}

/// Creates the format 31 schema, installs the statistics hints and sets `user_version`.
/// Runs in one transaction so a failure leaves an empty file, never a half schema.
pub fn create_schema(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().map_err(sql_err)?;
    tx.execute_batch(CREATE_SCHEMA_SQL).map_err(sql_err)?;
    install_statistics(&tx)?;
    tx.execute_batch(&format!("PRAGMA user_version = {FORMAT_WRITE};"))
        .map_err(sql_err)?;
    tx.commit().map_err(sql_err)
}

/// Installs the `sqlite_stat1` planner hints. `ANALYZE` also creates `sqlite_stat4` when the
/// linked SQLite is built with STAT4 (the bundled one is); svn's own SQLite usually is not, and
/// the table is useless for these hints, so it is dropped to keep the schema identical.
fn install_statistics(conn: &Connection) -> Result<()> {
    conn.execute_batch(INSTALL_STATISTICS_SQL)
        .map_err(sql_err)?;
    conn.execute_batch("DROP TABLE IF EXISTS sqlite_stat4;")
        .map_err(sql_err)
}

/// Re-installs `sqlite_stat1` hints when the table is missing (svn does this on open of format 31;
/// failures are ignored there too because the hints are only an optimisation).
pub fn ensure_statistics(conn: &Connection) -> Result<()> {
    let have: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_stat1'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if have.is_none() {
        // Optimisation only: ignore a failure exactly like svn_wc__db_install_schema_statistics.
        let _ = install_statistics(conn);
    }
    Ok(())
}
