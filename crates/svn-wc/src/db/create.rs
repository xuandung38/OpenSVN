//! Creation of a new administrative area, crash safe.
//!
//! `wc.db` appears under its final name only once it holds the complete schema and the root
//! rows: it is built as `.svn/tmp/wc.db.creating`, closed, and renamed into place. A process
//! killed before the rename leaves a `.svn` *skeleton* (`tmp`, maybe an empty `pristine`, and in
//! `tmp` only the half-built temp database, its journal and the creation lock file) that no
//! client takes for a working copy and that the next [`WcDb::create`] recognises and replaces,
//! unless the creation lock shows that its creator is still at work. A process killed after the rename
//! leaves a regular working copy whose root is `incomplete`, which an update finishes.
//!
//! Earlier versions created `wc.db` in place, so a kill in the first milliseconds left an empty
//! or schema-less `.svn/wc.db`. [`WcDb::open`] reports such a file as an interrupted checkout,
//! and [`WcDb::create`] treats it as part of a skeleton too.
//!
//! Only verified skeletons are ever removed, file by file (only the names listed above), and the
//! directories with a non-recursive `rmdir`, so anything unexpected that appears in between makes
//! the removal fail instead of deleting it.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use svn_types::{ErrorCode, Result, SvnError};

use super::connection::{OpenMode, open_connection, open_existing, sql_err};
use super::{ADM_DIR, DB_FILE, FORMAT_WRITE, LEGACY_FORMAT_MARKER, NewWc, WcDb, depth_word};
use super::{failpoint, schema};

/// Temp name of the database while it is built (inside `.svn/tmp`).
const CREATING_DB: &str = "wc.db.creating";
/// Its rollback journal (`TRUNCATE` mode leaves an empty one behind).
const CREATING_JOURNAL: &str = "wc.db.creating-journal";
/// Held (OS file lock) by the creating process for the whole creation.
const CREATING_LOCK: &str = "wc.db.creating.lock";
/// Journal of an in-place `wc.db` of earlier versions.
const DB_JOURNAL: &str = "wc.db-journal";

fn io_err(context: &str, path: &Path, err: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{context} '{}'", path.display()), err)
}

/// Whether `path` is a real directory (not a symlink).
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

/// Whether `path` is a regular file (not a symlink).
fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// True when the database at `path` holds nothing at all: an empty file, or (after SQLite rolled
/// back a hot journal) no schema object and `user_version` 0. Anything SQLite cannot read is
/// *not* reported empty.
pub(super) fn db_is_uninitialized(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() && m.len() == 0 => return true,
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

/// Files (to remove one by one) and directories (to `rmdir`, deepest first) of a verified
/// skeleton.
struct Skeleton {
    files: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    lock: Option<PathBuf>,
}

/// Inspects `adm`: `Some` when it is exactly the skeleton of an interrupted creation (see the
/// module documentation), `None` when it holds anything else.
fn inspect_skeleton(adm: &Path) -> Result<Option<Skeleton>> {
    if !real_dir(adm) {
        return Ok(None);
    }
    let mut sk = Skeleton {
        files: Vec::new(),
        dirs: Vec::new(),
        lock: None,
    };
    for entry in fs::read_dir(adm).map_err(|e| io_err("cannot read", adm, &e))? {
        let entry = entry.map_err(|e| io_err("cannot read", adm, &e))?;
        let path = entry.path();
        match entry.file_name().to_str() {
            Some("tmp") if real_dir(&path) => {
                for inner in fs::read_dir(&path).map_err(|e| io_err("cannot read", &path, &e))? {
                    let inner = inner.map_err(|e| io_err("cannot read", &path, &e))?;
                    let p = inner.path();
                    let name = inner.file_name();
                    let known = matches!(
                        name.to_str(),
                        Some(CREATING_DB | CREATING_JOURNAL | CREATING_LOCK)
                    );
                    if !known || !regular_file(&p) {
                        return Ok(None);
                    }
                    if name.to_str() == Some(CREATING_LOCK) {
                        sk.lock = Some(p);
                    } else {
                        sk.files.push(p);
                    }
                }
                sk.dirs.push(path);
            }
            Some("pristine") if real_dir(&path) => {
                let mut it = fs::read_dir(&path).map_err(|e| io_err("cannot read", &path, &e))?;
                if it.next().is_some() {
                    return Ok(None);
                }
                sk.dirs.push(path);
            }
            Some(DB_FILE) if db_is_uninitialized(&path) => sk.files.push(path),
            Some(DB_JOURNAL) if regular_file(&path) => sk.files.push(path),
            _ => return Ok(None),
        }
    }
    // A journal alone (no database next to it) is not something this engine leaves, and an
    // area without `tmp` (an empty `.svn`, say) carries no sign of being ours at all.
    let has = |name: &str| {
        sk.files
            .iter()
            .any(|p| p.file_name() == Some(name.as_ref()))
    };
    if (has(DB_JOURNAL) && !has(DB_FILE)) || !sk.dirs.iter().any(|d| d.ends_with("tmp")) {
        return Ok(None);
    }
    sk.dirs.push(adm.to_path_buf());
    Ok(Some(sk))
}

/// The OS lock on the creation lock file. Released on drop.
struct CreationLock {
    file: fs::File,
}

impl CreationLock {
    /// Opens the lock file (creating it when `create`) and takes it without waiting.
    /// `Ok(None)`: the file does not exist (only without `create`); `Ok(Some(None))`: another
    /// process holds it.
    fn try_take(path: &Path, create: bool) -> Result<Option<Option<CreationLock>>> {
        let opened = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(path);
        let file = match opened {
            Ok(f) => f,
            Err(e) if !create && e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("cannot open", path, &e)),
        };
        match file.try_lock() {
            Ok(()) => Ok(Some(Some(CreationLock { file }))),
            Err(fs::TryLockError::WouldBlock) => Ok(Some(None)),
            Err(fs::TryLockError::Error(e)) => Err(io_err("cannot lock", path, &e)),
        }
    }
}

impl Drop for CreationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Removes the skeleton of an interrupted creation in `adm`, after verifying it holds nothing
/// else and that no live process is still creating it. `Ok(false)`: `adm` is not a skeleton.
fn remove_skeleton(adm: &Path) -> Result<bool> {
    let Some(sk) = inspect_skeleton(adm)? else {
        return Ok(false);
    };
    let held = match &sk.lock {
        Some(p) => match CreationLock::try_take(p, false)? {
            Some(Some(l)) => Some(l),
            Some(None) => {
                return Err(SvnError::new(
                    ErrorCode::WcBusy,
                    format!(
                        "another process is creating a working copy in '{}'",
                        adm.parent().unwrap_or(adm).display()
                    ),
                ));
            }
            // Gone since the inspection: its creator just finished.
            None => return Ok(false),
        },
        None => None,
    };
    // Look again while holding the lock: a creator that finished in between has renamed a
    // complete database into place, which is no longer a skeleton.
    let Some(sk) = inspect_skeleton(adm)? else {
        return Ok(false);
    };
    for f in sk.files.iter().chain(sk.lock.iter()) {
        match fs::remove_file(f) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("cannot remove", f, &e)),
        }
    }
    drop(held);
    for d in &sk.dirs {
        // Non-recursive: fails when anything appeared since the inspection.
        fs::remove_dir(d).map_err(|e| io_err("cannot remove", d, &e))?;
    }
    Ok(true)
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
            "'{}' already has an administrative area without wc.db (pre-1.7 working copy or \
             unknown files); upgrade it with the svn command line client or remove it",
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
    /// Ownership: `.svn` is created with a non-recursive `mkdir`, which succeeds for exactly one
    /// caller; that caller then takes the creation lock and owns everything below it, removing
    /// it again on failure. An existing `.svn` is rejected without being touched, except the
    /// verified skeleton of an interrupted creation whose creator is gone, which is removed and
    /// created afresh (see the module documentation).
    pub fn create(wcroot: &Path, new: &NewWc) -> Result<WcDb> {
        let adm = wcroot.join(ADM_DIR);
        let mut replaced = false;
        loop {
            match fs::create_dir(&adm) {
                Ok(()) => break,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if !replaced && remove_skeleton(&adm)? {
                        replaced = true;
                        continue;
                    }
                    return Err(existing_adm(wcroot));
                }
                Err(e) => return Err(io_err("cannot create", &adm, &e)),
            }
        }
        // Until the lock is held, another creator may still take `.svn` for a skeleton and
        // replace it: never clean up on these errors, the directory may not be ours any more.
        let tmp = adm.join("tmp");
        fs::create_dir(&tmp).map_err(|e| io_err("cannot create", &tmp, &e))?;
        let lock_path = tmp.join(CREATING_LOCK);
        let lock = CreationLock::try_take(&lock_path, true)?
            .flatten()
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::WcBusy,
                    format!(
                        "another process is creating a working copy in '{}'",
                        wcroot.display()
                    ),
                )
            })?;
        let built = Self::populate_new(wcroot, &adm, new);
        if built.is_err() {
            // The lock was held throughout, so everything below `.svn` is this call's.
            drop(lock);
            let _ = fs::remove_dir_all(&adm);
            return built;
        }
        let _ = fs::remove_file(&lock_path);
        drop(lock);
        built
    }

    fn populate_new(wcroot: &Path, adm: &Path, new: &NewWc) -> Result<WcDb> {
        let pristine = adm.join("pristine");
        fs::create_dir(&pristine).map_err(|e| io_err("cannot create", &pristine, &e))?;
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
