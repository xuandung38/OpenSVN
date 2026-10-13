//! wc.db access layer (track A). [`WcDb`] is the only way other modules touch the database.
//!
//! One `WcDb` owns exactly one SQLite connection behind a mutex: all reads and writes of a
//! working copy from this process are serialized through it, and cross-process contention is
//! absorbed by the 10 s busy timeout svn uses. Write transactions are `BEGIN IMMEDIATE` and must
//! stay short: never hold [`WcDb::with_write`] across network I/O.

mod connection;
mod schema;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use svn_types::{Depth, ErrorCode, Result, SvnError};

pub use connection::{BUSY_TIMEOUT, OpenMode, open_connection, sql_err};
pub use schema::{FORMAT_MAX_READ, FORMAT_WRITE, check_supported_format, read_user_version};

/// Name of the administrative directory.
pub const ADM_DIR: &str = ".svn";
/// Database file inside [`ADM_DIR`].
pub const DB_FILE: &str = "wc.db";
/// Marker written to `.svn/format` and `.svn/entries` so pre-1.7 clients report "newer WC".
const LEGACY_FORMAT_MARKER: &str = "12\n";

/// Parameters for the root BASE node of a new working copy.
#[derive(Debug, Clone)]
pub struct NewWc {
    /// Repository root URL (URI-encoded).
    pub repos_root_url: String,
    /// Repository UUID.
    pub repos_uuid: String,
    /// Path of the checked-out directory inside the repository (no leading slash).
    pub repos_relpath: String,
    /// Initial revision; a root with revision > 0 is recorded `incomplete` until the first
    /// update fills it, like `svn_wc__db_init`.
    pub revision: u64,
    /// Depth of the root directory.
    pub depth: Depth,
}

/// Handle to one working copy's `wc.db`.
#[derive(Debug)]
pub struct WcDb {
    wcroot: PathBuf,
    wc_id: i64,
    format: u32,
    store_pristine: bool,
    conn: Mutex<Connection>,
}

/// Returns the working copy root containing `path`, searching `path` and then its ancestors for
/// `.svn/wc.db`. The nearest administrative area wins (nested checkouts are separate roots).
/// A pre-1.7 `.svn/entries` area without `wc.db` is reported as an unsupported format.
pub fn find_wcroot(path: &Path) -> Result<PathBuf> {
    let start = if path.is_dir() {
        Some(path)
    } else {
        path.parent()
    };
    let mut cur = start;
    while let Some(dir) = cur {
        let adm = dir.join(ADM_DIR);
        if adm.join(DB_FILE).is_file() {
            return Ok(dir.to_path_buf());
        }
        if adm.join("entries").is_file() {
            return Err(SvnError::new(
                ErrorCode::WcFormatUnsupported,
                format!(
                    "'{}' is a pre-1.7 working copy; upgrade it with the svn command line client",
                    dir.display()
                ),
            ));
        }
        cur = dir.parent();
    }
    Err(SvnError::new(
        ErrorCode::NotAWorkingCopy,
        format!("'{}' is not inside a working copy", path.display()),
    ))
}

fn depth_word(depth: Depth) -> &'static str {
    match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
        Depth::Exclude => "exclude",
        Depth::Unknown => "unknown",
    }
}

/// Error for a `.svn` that already exists when a creation was requested.
fn existing_adm(wcroot: &Path) -> SvnError {
    let adm = wcroot.join(ADM_DIR);
    if adm.join(DB_FILE).exists() {
        return SvnError::new(
            ErrorCode::Internal,
            format!(
                "'{}' already holds a working copy database",
                wcroot.display()
            ),
        );
    }
    SvnError::new(
        ErrorCode::WcFormatUnsupported,
        format!(
            "'{}' already has an administrative area without wc.db (pre-1.7 working copy or an \
             unfinished creation); upgrade it with the svn command line client or remove it",
            wcroot.display()
        ),
    )
}

fn io_err(context: &str, path: &Path, err: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{context} '{}'", path.display()), err)
}

impl WcDb {
    /// Creates a new format 31 working copy administrative area in `wcroot` (which must exist and
    /// must not contain a `.svn` entry of any kind): `.svn/{wc.db,format,entries,tmp,pristine}`
    /// plus the REPOSITORY, WCROOT and root BASE rows exactly as `svn_wc__db_init` inserts them.
    ///
    /// Ownership: `.svn` is created with a non-recursive `mkdir`, which succeeds for exactly one
    /// caller. That caller owns everything below it and removes it again on failure; every other
    /// caller (concurrent creator, existing wc-ng or pre-1.7 working copy, leftovers of a crashed
    /// creation) is rejected without touching the directory.
    pub fn create(wcroot: &Path, new: &NewWc) -> Result<WcDb> {
        let adm = wcroot.join(ADM_DIR);
        match fs::create_dir(&adm) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(existing_adm(wcroot));
            }
            Err(e) => return Err(io_err("cannot create", &adm, &e)),
        }
        let built = Self::populate_new(wcroot, &adm, new);
        if built.is_err() {
            // Only this call ever wrote below `.svn`, so removing it cannot hit a winner's files.
            let _ = fs::remove_dir_all(&adm);
        }
        built
    }

    fn populate_new(wcroot: &Path, adm: &Path, new: &NewWc) -> Result<WcDb> {
        for dir in [adm.join("tmp"), adm.join("pristine")] {
            fs::create_dir(&dir).map_err(|e| io_err("cannot create", &dir, &e))?;
        }
        let db = Self::build_new(wcroot, &adm.join(DB_FILE), new)?;
        for name in ["format", "entries"] {
            let p = adm.join(name);
            fs::write(&p, LEGACY_FORMAT_MARKER).map_err(|e| io_err("cannot write", &p, &e))?;
        }
        Ok(db)
    }

    fn build_new(wcroot: &Path, db_path: &Path, new: &NewWc) -> Result<WcDb> {
        let mut conn = open_connection(db_path, OpenMode::Create)?;
        schema::create_schema(&mut conn)?;
        let wc_id = {
            let tx = conn.transaction().map_err(sql_err)?;
            tx.execute(
                "INSERT INTO repository (root, uuid) VALUES (?1, ?2)",
                (&new.repos_root_url, &new.repos_uuid),
            )
            .map_err(sql_err)?;
            let repos_id = tx.last_insert_rowid();
            tx.execute("INSERT INTO wcroot (local_abspath) VALUES (NULL)", [])
                .map_err(sql_err)?;
            let wc_id = tx.last_insert_rowid();
            let revision = i64::try_from(new.revision)
                .map_err(|_| SvnError::new(ErrorCode::Internal, "revision out of range"))?;
            let presence = if new.revision > 0 {
                "incomplete"
            } else {
                "normal"
            };
            tx.execute(
                "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
                 repos_path, revision, presence, depth, kind) \
                 VALUES (?1, '', 0, NULL, ?2, ?3, ?4, ?5, ?6, 'dir')",
                (
                    wc_id,
                    repos_id,
                    &new.repos_relpath,
                    revision,
                    presence,
                    depth_word(new.depth),
                ),
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
            wc_id
        };
        Ok(WcDb {
            wcroot: wcroot.to_path_buf(),
            wc_id,
            format: FORMAT_WRITE,
            store_pristine: true,
            conn: Mutex::new(conn),
        })
    }

    /// Opens the working copy whose root is exactly `wcroot` (use [`find_wcroot`] first).
    /// Format 31 opens read/write; format 32 opens read only; other formats are rejected.
    pub fn open(wcroot: &Path) -> Result<WcDb> {
        let db_path = wcroot.join(ADM_DIR).join(DB_FILE);
        if !db_path.is_file() {
            return Err(SvnError::new(
                ErrorCode::NotAWorkingCopy,
                format!("working copy database '{}' not found", db_path.display()),
            ));
        }
        // Probe the format read-only first so an unsupported or newer WC is never touched.
        let format = {
            let probe = open_connection(&db_path, OpenMode::ReadOnly)?;
            let format = read_user_version(&probe)?;
            check_supported_format(format)?;
            format
        };
        let mode = if format == FORMAT_WRITE {
            OpenMode::ReadWrite
        } else {
            OpenMode::ReadOnly
        };
        let conn = open_connection(&db_path, mode)?;
        if format == FORMAT_WRITE {
            schema::ensure_statistics(&conn)?;
        }
        let wc_id: i64 = conn
            .query_row(
                "SELECT id FROM wcroot WHERE local_abspath IS NULL",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| SvnError::new(ErrorCode::Corrupt, "wc.db: missing row in WCROOT"))?;
        let store_pristine = if format >= 32 {
            conn.query_row(
                "SELECT store_pristine FROM settings WHERE wc_id = ?1",
                [wc_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map_err(sql_err)?
            .is_none_or(|v| v != 0)
        } else {
            true
        };
        Ok(WcDb {
            wcroot: wcroot.to_path_buf(),
            wc_id,
            format,
            store_pristine,
            conn: Mutex::new(conn),
        })
    }

    /// Absolute path of the working copy root.
    pub fn wcroot(&self) -> &Path {
        &self.wcroot
    }

    /// `WCROOT.id` of this working copy.
    pub fn wc_id(&self) -> i64 {
        self.wc_id
    }

    /// `PRAGMA user_version` found on open (31 or 32).
    pub fn format(&self) -> u32 {
        self.format
    }

    /// True when pristine texts are stored on disk (always true before format 32).
    pub fn store_pristine(&self) -> bool {
        self.store_pristine
    }

    /// True when this build may write the working copy (format 31 only).
    pub fn is_writable(&self) -> bool {
        self.format == FORMAT_WRITE
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic in another closure cannot corrupt SQLite state, so recover the guard.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs a read-only closure on the connection. Do not hold the result's borrows across calls.
    pub fn with_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.lock())
    }

    /// Runs `f` inside a `BEGIN IMMEDIATE` transaction; commits on `Ok`, rolls back on `Err`.
    /// Fails with [`ErrorCode::WcFormatUnsupported`] on format 32 working copies.
    pub fn with_write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        if !self.is_writable() {
            return Err(SvnError::new(
                ErrorCode::WcFormatUnsupported,
                format!(
                    "working copy format {} is not supported for writing in this version",
                    self.format
                ),
            ));
        }
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        let value = f(&tx)?;
        tx.commit().map_err(sql_err)?;
        Ok(value)
    }
}
