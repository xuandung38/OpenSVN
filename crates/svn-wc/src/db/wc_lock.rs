//! WC_LOCK handling: obtain/release directory write locks, recognise locks held by other
//! processes, recover locks left by crashed application processes.
//!
//! Row semantics are those of `svn_wc__db_wclock_obtain` (libsvn_wc/wc_db.c, 1.14.5 == 1.15.0):
//! a row `(dir, locked_levels)` covers `dir` and descendants up to `locked_levels` levels below
//! (`-1` = whole subtree). WC_LOCK rows carry no owner, so ownership is kept outside wc.db in the
//! application's [`LockOwnershipStore`]:
//!
//! * a lock we obtained has a DB row and an ownership record (pid + boot id);
//! * a row without a record (svn CLI, TortoiseSVN, an older crash) is *foreign*: it is reported as
//!   [`LockState::HeldByOther`] and never removed except by an explicit [`WcDb::wc_lock_steal`];
//! * a record whose owner is dead identifies a row we may remove during
//!   [`WcDb::wc_cleanup_locks`].
//!
//! Known limit: if an app crash is followed by `svn cleanup` and a *new* svn operation locks the
//! same directory before our cleanup runs, the dead record still points at that directory and
//! cleanup would remove svn's live lock. WC_LOCK has no owner column to tell the two apart.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};
use svn_types::{ErrorCode, LockOwnershipStore, RelPath, Result, SvnError};

use super::WcDb;
use super::connection::sql_err;
use super::nodes::{relpath_depth, strict_descendant};

/// Who owns the locks this process takes.
#[derive(Clone)]
pub struct LockOwner {
    /// Application ownership registry (`app.sqlite` in production).
    pub store: Arc<dyn LockOwnershipStore>,
    /// Process id of this process.
    pub pid: u32,
    /// Boot session id (pid reuse across reboots must not look alive).
    pub boot_id: String,
}

impl std::fmt::Debug for LockOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockOwner")
            .field("pid", &self.pid)
            .field("boot_id", &self.boot_id)
            .finish()
    }
}

/// How a directory is locked, from this handle's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// No row covers the directory.
    Free,
    /// Covered by a lock obtained through this handle.
    HeldByUs,
    /// Covered by a row we did not take: another process, svn CLI, TortoiseSVN, or a stale lock.
    HeldByOther,
}

/// Whether obtaining the lock requires an empty work queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObtainMode {
    /// Normal operation: pending work items mean the working copy needs cleanup first.
    Normal,
    /// Cleanup itself: lock regardless of pending work items.
    Cleanup,
}

fn busy(msg: String) -> SvnError {
    SvnError::new(ErrorCode::WcBusy, msg)
}

impl WcDb {
    /// Registers the ownership registry used by every lock operation of this handle.
    pub fn set_lock_owner(&self, owner: LockOwner) {
        *self
            .lock_owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(owner);
    }

    fn owner(&self) -> Result<LockOwner> {
        self.lock_owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::Internal,
                    "no lock owner registered; call set_lock_owner first",
                )
            })
    }

    fn owned(&self) -> MutexGuard<'_, std::collections::BTreeMap<RelPath, i32>> {
        self.owned_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Canonical root, as the ownership store expects.
    fn canonical_root(&self) -> Result<PathBuf> {
        std::fs::canonicalize(&self.wcroot).map_err(|e| {
            SvnError::from_io(format!("cannot resolve '{}'", self.wcroot.display()), &e)
        })
    }

    /// Takes the WC lock on `dir` covering `levels` levels below it (`-1` = subtree).
    ///
    /// Errors: [`ErrorCode::WcBusy`] when any existing row (ours or foreign) overlaps, unless it
    /// lies below `dir` and is already ours (a lock may be extended upward);
    /// [`ErrorCode::WcNeedsCleanup`] when work items are pending in [`ObtainMode::Normal`];
    /// [`ErrorCode::PathNotFound`] when `dir` is not versioned.
    pub fn wc_lock_obtain(&self, dir: &RelPath, levels: i32, mode: ObtainMode) -> Result<()> {
        if levels < -1 {
            return Err(SvnError::new(ErrorCode::Internal, "levels must be >= -1"));
        }
        let owner = self.owner()?;
        if !self.is_writable() {
            return Err(SvnError::new(
                ErrorCode::WcFormatUnsupported,
                format!(
                    "working copy format {} is not supported for writing in this version",
                    self.format
                ),
            ));
        }
        let root = self.canonical_root()?;
        let depth = relpath_depth(dir);
        if let Some((held, _)) = self.owned().iter().find(|(held, lv)| {
            dir.starts_with(held)
                && (**lv == -1
                    || i64::from(**lv) + i64::from(relpath_depth(held)) >= i64::from(depth))
        }) {
            return Err(busy(format!("'{dir}' is already locked via '{held}'")));
        }
        let owned_snapshot: BTreeSet<RelPath> = self.owned().keys().cloned().collect();
        // Cheap rejection first, so a refused attempt never disturbs an existing (possibly dead
        // but still recoverable) ownership record.
        self.with_read(|c| check_obtainable(c, self.wc_id, dir, levels, mode, &owned_snapshot))?;
        // Record before the row: a crash between the two steps leaves a harmless orphan record,
        // never a row nobody can attribute.
        owner.store.record(&root, dir, owner.pid, &owner.boot_id)?;
        let result =
            self.with_write(|tx| obtain_in_txn(tx, self.wc_id, dir, levels, mode, &owned_snapshot));
        match result {
            Ok(()) => {
                self.owned().insert(dir.clone(), levels);
                Ok(())
            }
            Err(e) => {
                let _ = owner.store.forget(&root, dir);
                Err(e)
            }
        }
    }

    /// Releases a lock obtained through this handle.
    pub fn wc_lock_release(&self, dir: &RelPath) -> Result<()> {
        let owner = self.owner()?;
        if self.owned().remove(dir).is_none() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("working copy not locked at '{dir}'"),
            ));
        }
        self.with_write(|tx| delete_row(tx, self.wc_id, dir).map(|_| ()))?;
        owner.store.forget(&self.canonical_root()?, dir)
    }

    /// Lock state of `dir` (a row on `dir` or on an ancestor whose levels reach it).
    pub fn wc_lock_held(&self, dir: &RelPath) -> Result<LockState> {
        let depth = relpath_depth(dir);
        let ours = self.owned().iter().any(|(held, lv)| {
            dir.starts_with(held)
                && (*lv == -1
                    || i64::from(*lv) + i64::from(relpath_depth(held)) >= i64::from(depth))
        });
        if ours {
            return Ok(LockState::HeldByUs);
        }
        Ok(
            match self.with_read(|c| find_covering_lock(c, self.wc_id, dir))? {
                Some(_) => LockState::HeldByOther,
                None => LockState::Free,
            },
        )
    }

    /// Removes the lock row on exactly `dir` (and its ownership record) on the user's explicit
    /// confirmation. Returns whether a row existed.
    pub fn wc_lock_steal(&self, dir: &RelPath) -> Result<bool> {
        let owner = self.owner()?;
        let removed = self.with_write(|tx| delete_row(tx, self.wc_id, dir))?;
        self.owned().remove(dir);
        owner.store.forget(&self.canonical_root()?, dir)?;
        Ok(removed)
    }

    /// Recovers locks of dead application processes and drops lock rows whose directory is no
    /// longer versioned. Locks of live processes and foreign rows with a versioned directory are
    /// left alone. Returns the directories whose rows were removed.
    pub fn wc_cleanup_locks(&self) -> Result<Vec<RelPath>> {
        let owner = self.owner()?;
        let root = self.canonical_root()?;
        let dead = owner.store.owned_by_dead_process(&root)?;
        let mut removed = Vec::new();
        for dir in &dead {
            if self.owned().contains_key(dir) {
                continue; // paranoia: never touch what this handle holds
            }
            if self.with_write(|tx| delete_row(tx, self.wc_id, dir))? {
                removed.push(dir.clone());
            }
            owner.store.forget(&root, dir)?;
        }
        let orphans = self.with_write(|tx| {
            let mut stmt = tx
                .prepare(
                    "SELECT local_dir_relpath FROM wc_lock WHERE wc_id = ?1 AND local_dir_relpath != '' \
                     AND NOT EXISTS (SELECT 1 FROM nodes WHERE nodes.wc_id = ?1 \
                                     AND nodes.local_relpath = wc_lock.local_dir_relpath)",
                )
                .map_err(sql_err)?;
            let dirs: Vec<String> = stmt
                .query_map([self.wc_id], |r| r.get(0))
                .map_err(sql_err)?
                .collect::<std::result::Result<_, _>>()
                .map_err(sql_err)?;
            for d in &dirs {
                tx.execute("DELETE FROM wc_lock WHERE wc_id = ?1 AND local_dir_relpath = ?2", params![self.wc_id, d])
                    .map_err(sql_err)?;
            }
            Ok(dirs)
        })?;
        for dir in orphans {
            let dir = super::nodes::rel(dir)?;
            owner.store.forget(&root, &dir)?;
            removed.push(dir);
        }
        removed.sort();
        removed.dedup();
        Ok(removed)
    }
}

fn delete_row(conn: &Connection, wc_id: i64, dir: &RelPath) -> Result<bool> {
    let n = conn
        .execute(
            "DELETE FROM wc_lock WHERE wc_id = ?1 AND local_dir_relpath = ?2",
            params![wc_id, dir.as_str()],
        )
        .map_err(sql_err)?;
    Ok(n > 0)
}

/// Nearest row on `dir` or an ancestor whose levels reach `dir` (svn's `find_wclock`).
fn find_covering_lock(conn: &Connection, wc_id: i64, dir: &RelPath) -> Result<Option<RelPath>> {
    let depth = relpath_depth(dir);
    let mut cur = Some(dir.clone());
    while let Some(candidate) = cur {
        let levels: Option<i64> = conn
            .query_row(
                "SELECT locked_levels FROM wc_lock WHERE wc_id = ?1 AND local_dir_relpath = ?2",
                params![wc_id, candidate.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(levels) = levels
            && (levels == -1 || levels + i64::from(relpath_depth(&candidate)) >= i64::from(depth))
        {
            return Ok(Some(candidate));
        }
        cur = candidate.parent();
    }
    Ok(None)
}

/// All reasons a lock cannot be taken, without changing anything.
fn check_obtainable(
    conn: &Connection,
    wc_id: i64,
    dir: &RelPath,
    levels: i32,
    mode: ObtainMode,
    owned: &BTreeSet<RelPath>,
) -> Result<()> {
    if !dir.is_root() {
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 LIMIT 1",
                params![wc_id, dir.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_none() {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("the node '{dir}' was not found"),
            ));
        }
    }
    if mode == ObtainMode::Normal {
        let pending: i64 = conn
            .query_row("SELECT count(*) FROM work_queue", [], |r| r.get(0))
            .map_err(sql_err)?;
        if pending > 0 {
            return Err(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                "the working copy has unfinished work; run cleanup first",
            ));
        }
    }
    // Locks below the new root that are not ours block it (ours may be extended).
    let max_depth = i64::from(relpath_depth(dir)) + i64::from(levels);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT local_dir_relpath FROM wc_lock WHERE wc_id = ?1 AND {}",
            strict_descendant("local_dir_relpath", "?2")
        ))
        .map_err(sql_err)?;
    let below: Vec<String> = stmt
        .query_map(params![wc_id, dir.as_str()], |r| r.get(0))
        .map_err(sql_err)?
        .collect::<std::result::Result<_, _>>()
        .map_err(sql_err)?;
    for row in below {
        let row = super::nodes::rel(row)?;
        if levels >= 0 && i64::from(relpath_depth(&row)) > max_depth {
            continue;
        }
        if !owned.contains(&row) {
            return Err(busy(format!(
                "working copy '{dir}' locked: '{row}' is already locked"
            )));
        }
    }
    // Existing row on the directory or above that reaches it.
    if let Some(lock) = find_covering_lock(conn, wc_id, dir)? {
        return Err(busy(format!(
            "working copy '{dir}' locked: '{lock}' is already locked"
        )));
    }
    Ok(())
}

fn obtain_in_txn(
    conn: &Connection,
    wc_id: i64,
    dir: &RelPath,
    levels: i32,
    mode: ObtainMode,
    owned: &BTreeSet<RelPath>,
) -> Result<()> {
    check_obtainable(conn, wc_id, dir, levels, mode, owned)?;
    conn.execute(
        "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (?1, ?2, ?3)",
        params![wc_id, dir.as_str(), levels],
    )
    .map_err(|e| busy(format!("failed to lock working copy '{dir}': {e}")))?;
    Ok(())
}
