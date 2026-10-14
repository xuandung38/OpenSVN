//! Creation of a new administrative area, crash safe.
//!
//! `wc.db` appears under its final name only once it holds the complete schema and the root
//! rows: it is built as `.svn/tmp/wc.db.creating`, closed, and renamed into place. A process
//! killed before the rename leaves a `.svn` *skeleton* that no client takes for a working copy:
//! `tmp` holding the creation lock file `wc.db.creating.lock` and maybe the half-built database
//! and its journal, and maybe an empty `pristine`. The next [`WcDb::create`] takes the skeleton
//! over in place, unless its creator still holds the lock. A process killed after the rename
//! leaves a regular working copy whose root is `incomplete`, which an update finishes.
//!
//! Earlier versions created `wc.db` in place, so a kill in the first milliseconds left an empty
//! or schema-less `.svn/wc.db` (plus `tmp` and `pristine`). [`WcDb::open`] reports such a file as
//! an interrupted checkout, and [`WcDb::create`] takes that skeleton over too.
//!
//! # Ownership
//! Whoever holds the OS lock on `.svn/tmp/wc.db.creating.lock` owns `.svn`. A creator makes
//! `.svn` and `tmp` with exclusive `mkdir`s, then creates and locks the lock file; until it holds
//! the lock it never deletes anything. Another caller takes a skeleton over only when it holds
//! that same lock (checked to still be the file at that path) and the skeleton, inspected again
//! under the lock, holds nothing but the names above. It then deletes only those files, one by
//! one, never a directory, and reuses `tmp` and `pristine`. The lock file itself is deleted only
//! by its holder: after the database is in place, or with the whole `.svn` when the creation
//! fails. Anything else in `.svn` (user files, svn's own files, a complete or unreadable
//! `wc.db`) makes the area foreign: it is rejected and never touched.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use svn_types::{ErrorCode, Result, SvnError};

use super::connection::{OpenMode, open_connection, open_existing, sql_err};
use super::{ADM_DIR, DB_FILE, FORMAT_WRITE, LEGACY_FORMAT_MARKER, NewWc, WcDb, depth_word};
use super::{failpoint, schema};

/// Temp name of the database while it is built (inside `.svn/tmp`). Every file of the creation
/// starts with it.
pub(super) const CREATING_DB: &str = "wc.db.creating";
/// Its rollback journal (`TRUNCATE` mode leaves an empty one behind).
const CREATING_JOURNAL: &str = "wc.db.creating-journal";
/// Held (OS file lock) by the creating process for the whole creation.
const CREATING_LOCK: &str = "wc.db.creating.lock";
/// Journal of an in-place `wc.db` of earlier versions.
const DB_JOURNAL: &str = "wc.db-journal";

fn io_err(context: &str, path: &Path, err: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{context} '{}'", path.display()), err)
}

fn busy(wcroot: &Path) -> SvnError {
    SvnError::new(
        ErrorCode::WcBusy,
        format!(
            "another process is creating a working copy in '{}'",
            wcroot.display()
        ),
    )
}

/// Whether `path` is a real directory (not a symlink).
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// Whether `path` is a regular file (not a symlink).
fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// True when the database at `path` holds nothing at all: an empty file (with no journal to roll
/// back), or (after SQLite rolled back a hot journal) no schema object and `user_version` 0.
/// Anything SQLite cannot read is *not* reported empty.
pub(super) fn db_is_uninitialized(path: &Path) -> bool {
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    // A non-empty journal could hold pages to roll back: then SQLite has to look.
    let journal_empty = fs::symlink_metadata(Path::new(&journal)).map_or(true, |m| m.len() == 0);
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() && m.len() == 0 && journal_empty => return true,
        Ok(m) if m.file_type().is_file() => {}
        _ => return false,
    }
    let Ok((conn, _)) = open_existing(path) else {
        return false;
    };
    let version: rusqlite::Result<i64> = conn.query_row("PRAGMA user_version", [], |r| r.get(0));
    let objects: rusqlite::Result<i64> =
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0));
    matches!((version, objects), (Ok(0), Ok(0)))
}

/// Files of a verified skeleton that a takeover deletes (everything but the lock file).
struct Skeleton {
    junk: Vec<PathBuf>,
}

/// Inspects `adm`: `Some` when it is exactly the skeleton of an interrupted creation (see the
/// module documentation), `None` when it holds anything else.
fn inspect_skeleton(adm: &Path) -> Result<Option<Skeleton>> {
    if !real_dir(adm) {
        return Ok(None);
    }
    let mut junk = Vec::new();
    let (mut has_tmp, mut has_lock, mut has_db, mut has_db_journal) = (false, false, false, false);
    for entry in fs::read_dir(adm).map_err(|e| io_err("cannot read", adm, &e))? {
        let entry = entry.map_err(|e| io_err("cannot read", adm, &e))?;
        let path = entry.path();
        match entry.file_name().to_str() {
            Some("tmp") if real_dir(&path) => {
                has_tmp = true;
                for inner in fs::read_dir(&path).map_err(|e| io_err("cannot read", &path, &e))? {
                    let inner = inner.map_err(|e| io_err("cannot read", &path, &e))?;
                    let p = inner.path();
                    if !regular_file(&p) {
                        return Ok(None);
                    }
                    match inner.file_name().to_str() {
                        Some(CREATING_LOCK) => has_lock = true,
                        Some(CREATING_DB | CREATING_JOURNAL) => junk.push(p),
                        _ => return Ok(None),
                    }
                }
            }
            Some("pristine") if real_dir(&path) => {
                let mut it = fs::read_dir(&path).map_err(|e| io_err("cannot read", &path, &e))?;
                if it.next().is_some() {
                    return Ok(None);
                }
            }
            Some(DB_FILE) if db_is_uninitialized(&path) => {
                has_db = true;
                junk.push(path);
            }
            Some(DB_JOURNAL) if regular_file(&path) => {
                has_db_journal = true;
                junk.push(path);
            }
            _ => return Ok(None),
        }
    }
    // Ours only with the evidence of a creation: the lock file of this version, or the empty
    // in-place database of earlier ones. An empty `.svn`, or a `tmp` alone, proves nothing.
    if !has_tmp || !(has_lock || has_db) || (has_db_journal && !has_db) {
        return Ok(None);
    }
    Ok(Some(Skeleton { junk }))
}

/// The OS lock on `.svn/tmp/wc.db.creating.lock`. Released on drop.
struct CreationLock {
    file: fs::File,
    path: PathBuf,
}

impl CreationLock {
    /// Takes the lock file in `tmp` without waiting, creating it when it does not exist.
    /// [`ErrorCode::WcBusy`] when another process holds it, or when the file at that path
    /// changed while it was being taken.
    fn take(tmp: &Path, wcroot: &Path) -> Result<CreationLock> {
        let path = tmp.join(CREATING_LOCK);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io_err("cannot open", &path, &e))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(busy(wcroot)),
            // A filesystem without file locks (some network shares): a checkout must still work
            // there; only concurrent creators in one directory lose their protection.
            Err(fs::TryLockError::Error(e)) => {
                tracing::debug!(error = %e, path = %path.display(), "creation lock unavailable");
            }
        }
        let lock = CreationLock { file, path };
        // The previous holder may have deleted the file (with `.svn`, or after finishing) between
        // our open and our lock: then we hold a lock on a file nobody else can see.
        if !lock.still_at_path() {
            return Err(busy(wcroot));
        }
        Ok(lock)
    }

    #[cfg(unix)]
    fn still_at_path(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        match (self.file.metadata(), fs::symlink_metadata(&self.path)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }

    /// Windows keeps the name of a deleted file that is still open (delete pending), so the
    /// path cannot be reused while we hold the handle; a deleted file is reported as missing.
    #[cfg(not(unix))]
    fn still_at_path(&self) -> bool {
        regular_file(&self.path)
    }
}

impl Drop for CreationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Error for a `.svn` that already exists and is not a skeleton.
fn existing_adm(wcroot: &Path) -> SvnError {
    let adm = wcroot.join(ADM_DIR);
    if adm.join(DB_FILE).exists() {
        return SvnError::new(
            ErrorCode::Internal,
            format!(
                "'{}' already holds a working copy database; to finish an interrupted checkout \
                 of the same URL, update that working copy",
                wcroot.display()
            ),
        );
    }
    SvnError::new(
        ErrorCode::WcFormatUnsupported,
        format!(
            "'{}' already has an administrative area without wc.db (pre-1.7 working copy, \
             another checkout starting, or unknown files); upgrade it with the svn command line \
             client or remove it",
            wcroot.display()
        ),
    )
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

impl WcDb {
    /// Creates a new format 31 working copy administrative area in `wcroot` (which must exist):
    /// `.svn/{wc.db,format,entries,tmp,pristine}` plus the REPOSITORY, WCROOT and root BASE rows
    /// exactly as `svn_wc__db_init` inserts them.
    ///
    /// An existing `.svn` is rejected without being touched, except the verified skeleton of an
    /// interrupted creation whose creator is gone, which is taken over (see the module
    /// documentation for the ownership rules). On failure, the caller that holds the creation
    /// lock removes `.svn` again.
    pub fn create(wcroot: &Path, new: &NewWc) -> Result<WcDb> {
        let adm = wcroot.join(ADM_DIR);
        let tmp = adm.join("tmp");
        let fresh = match fs::create_dir(&adm) {
            Ok(()) => true,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => false,
            Err(e) => return Err(io_err("cannot create", &adm, &e)),
        };
        if fresh {
            // Not ours to clean up until the lock is held: never delete on these errors.
            fs::create_dir(&tmp).map_err(|e| io_err("cannot create", &tmp, &e))?;
        } else if inspect_skeleton(&adm)?.is_none() {
            return Err(existing_adm(wcroot));
        }
        let lock = CreationLock::take(&tmp, wcroot)?;
        // Look again under the lock: another caller may have taken our fresh area over before we
        // held the lock, or finished or failed in between.
        let Some(sk) = inspect_skeleton(&adm)? else {
            // We hold the lock: nobody can need the lock file any more.
            let _ = fs::remove_file(&lock.path);
            return Err(existing_adm(wcroot));
        };
        for f in &sk.junk {
            match fs::remove_file(f) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(io_err("cannot remove", f, &e)),
            }
        }
        let built = Self::populate_new(wcroot, &adm, new);
        if built.is_err() {
            // We hold the lock, so everything below `.svn` is this call's.
            let _ = fs::remove_dir_all(&adm);
            drop(lock);
            return built;
        }
        let _ = fs::remove_file(&lock.path);
        drop(lock);
        built
    }

    fn populate_new(wcroot: &Path, adm: &Path, new: &NewWc) -> Result<WcDb> {
        let pristine = adm.join("pristine");
        match fs::create_dir(&pristine) {
            Ok(()) => {}
            // Only a verified skeleton's empty `pristine` (a symlink is not `real_dir`).
            Err(e) if e.kind() == ErrorKind::AlreadyExists && real_dir(&pristine) => {}
            Err(e) => return Err(io_err("cannot create", &pristine, &e)),
        }
        let tmp = adm.join("tmp");
        let building = tmp.join(CREATING_DB);
        let wc_id = {
            let mut conn = open_connection(&building, OpenMode::Create)?;
            failpoint!("wc.create.before_schema");
            let wc_id = Self::build_new(&mut conn, new)?;
            conn.close().map_err(|(_, e)| sql_err(e))?;
            wc_id
        };
        // The journal of a committed `TRUNCATE` transaction is empty; it must not linger in
        // `.svn/tmp` (cleanup would leave it, svn would not care).
        match fs::remove_file(tmp.join(CREATING_JOURNAL)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("cannot remove", &tmp.join(CREATING_JOURNAL), &e)),
        }
        failpoint!("wc.create.before_rename");
        let db_path = adm.join(DB_FILE);
        fs::rename(&building, &db_path).map_err(|e| io_err("cannot rename to", &db_path, &e))?;
        sync_dir(adm);
        for name in ["format", "entries"] {
            let p = adm.join(name);
            fs::write(&p, LEGACY_FORMAT_MARKER).map_err(|e| io_err("cannot write", &p, &e))?;
        }
        let conn = open_connection(&db_path, OpenMode::ReadWrite)?;
        Ok(WcDb {
            wcroot: wcroot.to_path_buf(),
            wc_id,
            format: FORMAT_WRITE,
            writable: true,
            store_pristine: true,
            conn: Mutex::new(conn),
            owned_locks: Mutex::new(Default::default()),
            lock_owner: Mutex::new(None),
            lock_gate: Mutex::new(()),
        })
    }

    /// Schema plus the REPOSITORY, WCROOT and root BASE rows; returns `WCROOT.id`.
    fn build_new(conn: &mut Connection, new: &NewWc) -> Result<i64> {
        schema::create_schema(conn)?;
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
        Ok(wc_id)
    }
}

#[cfg(test)]
mod tests;
