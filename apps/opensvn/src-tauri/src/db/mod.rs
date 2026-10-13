//! SQLite storage owned by the app: `app.sqlite` (durable state) and `cache.sqlite`
//! (disposable cache). WAL is allowed for these two files only; a working copy's `wc.db` is never
//! opened here.

mod app_db;
mod cache_db;
mod clock;
mod data_dir;
mod migrations;
mod wc_rows;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use opensvn_ipc::AppError;
use rusqlite::Connection;

pub use app_db::AppDb;
pub use cache_db::CacheDb;
pub use clock::now_iso;
pub use data_dir::current_data_dir;
pub use wc_rows::WcRow;

/// Storage failure.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database file was written by a newer app version.
    #[error("database schema version {found} is newer than the supported {supported}")]
    NewerSchema {
        /// Version found in the file.
        found: u32,
        /// Newest version this build understands.
        supported: u32,
    },
    /// The data directory could not be prepared.
    #[error("cannot prepare data directory: {0}")]
    Io(#[from] std::io::Error),
    /// A stored value could not be decoded.
    #[error("stored value is invalid: {0}")]
    Corrupt(String),
}

impl From<DbError> for AppError {
    fn from(e: DbError) -> Self {
        tracing::error!(error = %e, "database failure");
        AppError::internal("The application database failed.").with_detail(e.to_string())
    }
}

/// Shared SQLite connection. Statements are short, so a mutex is enough; async callers must not
/// hold the guard across an await point (the closure API makes that impossible).
#[derive(Clone)]
pub(crate) struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// Opens (creating if needed) a WAL-mode database at `path`.
    pub(crate) fn open(path: &Path, migrations: &[migrations::Migration]) -> Result<Self, DbError> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::migrate(&mut conn, migrations)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// In-memory database for tests.
    #[cfg(test)]
    pub(crate) fn open_in_memory(migrations: &[migrations::Migration]) -> Result<Self, DbError> {
        let mut conn = Connection::open_in_memory()?;
        migrations::migrate(&mut conn, migrations)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// Runs `f` with exclusive access to the connection.
    pub(crate) fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut guard: MutexGuard<'_, Connection> =
            self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}
