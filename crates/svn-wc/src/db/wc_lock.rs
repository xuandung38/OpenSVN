//! WC_LOCK handling: obtain/release directory write locks, recognise locks held by other
//! processes, recover locks left by crashed application processes.
//!
//! Row semantics are those of `svn_wc__db_wclock_obtain` (libsvn_wc/wc_db.c, 1.14.5 == 1.15.0):
//! a row `(dir, locked_levels)` covers `dir` and descendants up to `locked_levels` levels below
//! (`-1` = whole subtree).
//!
//! # Ownership and the stale-lock problem
//! WC_LOCK rows carry no owner, and svn takes exactly the same `('', -1)` row we do, so a row
//! alone can never prove that *we* left it behind. A lock obtained here therefore leaves three
//! traces, written in this order (a crash between steps always fails safe, towards "foreign"):
//!
//! 1. the WC_LOCK row (committed first);
//! 2. a record in the application's [`LockOwnershipStore`] (pid + boot id);
//! 3. a marker file `.svn/tmp/opensvn-lock-<nonce>` holding `(pid boot-id dir levels)` as a skel.
//!
//! `wc_cleanup_locks` removes a row only when a dead owner's record **and** a matching marker
//! exist. `svn cleanup` (the only svn path that clears other processes' rows) also empties
//! `.svn/tmp`, so after it the marker is gone and a later svn lock on the same directory is never
//! mistaken for ours; ordinary svn operations leave unknown tmp files alone. A row without a
//! marker (svn CLI, TortoiseSVN, a crash before step 3) is *foreign*: it is reported as
//! [`LockState::HeldByOther`], blocks with [`ErrorCode::WcBusy`] and is removed only by an
//! explicit [`WcDb::wc_lock_steal`] or by the user's `svn cleanup`. Nothing else ever deletes a
//! row that has no record or marker, in particular rows of unversioned directories.
//!
//! Residual risk: TortoiseSVN "Break write locks" uses the same `svn_wc_cleanup4`, so it should
//! also clear `.svn/tmp`; that is unverified on Windows and tracked as a follow-up for the
//! integration phase. Run `wc_cleanup_locks` when a working copy is opened (see
//! [`WcDb::open_and_recover`]) to keep the window between crash and recovery small.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};
use svn_types::{ErrorCode, LockOwnershipStore, RelPath, Result, SvnError};

use super::connection::sql_err;
use super::nodes::{relpath_depth, strict_descendant};
use super::{ADM_DIR, WcDb};
use crate::skel::Skel;

const MARKER_PREFIX: &str = "opensvn-lock-";

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

/// Result of [`WcDb::wc_cleanup_locks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockCleanup {
    /// Directories whose stale lock row (dead owner, record and marker present) was removed.
    pub recovered: Vec<RelPath>,
    /// Directories with a dead owner's record but no marker: the row, if any, is treated as
    /// foreign and left in place; the user must run cleanup or break the lock explicitly.
    pub needs_user_action: Vec<RelPath>,
}

fn busy(msg: String) -> SvnError {
    SvnError::new(ErrorCode::WcBusy, msg)
}

fn io(context: &str, path: &Path, e: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{context} '{}'", path.display()), e)
}

/// Contents of a marker file.
#[derive(Debug, PartialEq, Eq)]
struct Marker {
    pid: u32,
    boot_id: String,
    dir: String,
    levels: i32,
}

impl Marker {
    fn encode(&self) -> Vec<u8> {
        Skel::list(vec![
            Skel::atom(self.pid.to_string().into_bytes()),
            Skel::atom(self.boot_id.clone().into_bytes()),
            Skel::atom(self.dir.clone().into_bytes()),
            Skel::atom(self.levels.to_string().into_bytes()),
        ])
        .unparse()
    }

    fn decode(bytes: &[u8]) -> Option<Marker> {
        let skel = Skel::parse(bytes).ok()?;
        let [pid, boot, dir, levels] = skel.as_list()? else {
            return None;
        };
        let text = |s: &Skel| s.as_atom().and_then(|b| String::from_utf8(b.to_vec()).ok());
        Some(Marker {
            pid: text(pid)?.parse().ok()?,
            boot_id: text(boot)?,
            dir: text(dir)?,
            levels: text(levels)?.parse().ok()?,
        })
    }
}

impl WcDb {
    /// Registers the ownership registry used by every lock operation of this handle.
    pub fn set_lock_owner(&self, owner: LockOwner) {
        *self
            .lock_owner
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(owner);
    }

    /// Opens the working copy, registers the lock owner and immediately recovers locks left by
    /// dead application processes, so the window between a crash and its recovery stays small.
    /// Returns the handle and what the recovery did.
    pub fn open_and_recover(wcroot: &Path, owner: LockOwner) -> Result<(WcDb, LockCleanup)> {
        let db = WcDb::open(wcroot)?;
        db.set_lock_owner(owner);
        let report = if db.is_writable() {
            db.wc_cleanup_locks()?
        } else {
            LockCleanup::default()
        };
        Ok((db, report))
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

    fn gate(&self) -> MutexGuard<'_, ()> {
        self.lock_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Canonical root, as the ownership store expects.
    fn canonical_root(&self) -> Result<PathBuf> {
        std::fs::canonicalize(&self.wcroot).map_err(|e| io("cannot resolve", &self.wcroot, &e))
    }

    fn tmp_dir(&self) -> PathBuf {
        self.wcroot.join(ADM_DIR).join("tmp")
    }

    /// Every marker file with its parsed contents (unparsable files are not ours: ignored).
    fn markers(&self) -> Result<Vec<(PathBuf, Marker)>> {
        let dir = self.tmp_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io("cannot read", &dir, &e)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with(MARKER_PREFIX) {
                continue;
            }
            if let Ok(bytes) = std::fs::read(entry.path())
                && let Some(marker) = Marker::decode(&bytes)
            {
                out.push((entry.path(), marker));
            }
        }
        Ok(out)
    }

    fn write_marker(&self, marker: &Marker) -> Result<PathBuf> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = self.tmp_dir();
        std::fs::create_dir_all(&dir).map_err(|e| io("cannot create", &dir, &e))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = dir.join(format!(
            "{MARKER_PREFIX}{:x}-{}-{}",
            nanos,
            marker.pid,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| io("cannot create", &path, &e))?;
        file.write_all(&marker.encode())
            .map_err(|e| io("cannot write", &path, &e))?;
        file.sync_all().map_err(|e| io("cannot sync", &path, &e))?;
        Ok(path)
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
        let _gate = self.gate();
        let depth = relpath_depth(dir);
        if let Some((held, _)) = self.owned().iter().find(|(held, lv)| {
            dir.starts_with(held)
                && (**lv == -1
                    || i64::from(**lv) + i64::from(relpath_depth(held)) >= i64::from(depth))
        }) {
            return Err(busy(format!("'{dir}' is already locked via '{held}'")));
        }
        let owned_snapshot: BTreeSet<RelPath> = self.owned().keys().cloned().collect();
        // 1. the row, committed first: a crash from here on leaves a row nobody claims, which is
        //    treated as foreign (the safe direction).
        self.with_write(|tx| obtain_in_txn(tx, self.wc_id, dir, levels, mode, &owned_snapshot))
            .map_err(stale_hint)?;
        // 2. ownership record. Failing here (another live owner holds the record) means we must
        //    take our own row back; we never touch a record that is not ours.
        if let Err(e) = owner.store.record(&root, dir, owner.pid, &owner.boot_id) {
            let _ = self.with_write(|tx| delete_row(tx, self.wc_id, dir).map(|_| ()));
            return Err(e);
        }
        // 3. marker file, so cleanup can tell our stale row from a later svn lock.
        let marker = Marker {
            pid: owner.pid,
            boot_id: owner.boot_id.clone(),
            dir: dir.as_str().to_owned(),
            levels,
        };
        if let Err(e) = self.write_marker(&marker) {
            let _ = self.with_write(|tx| delete_row(tx, self.wc_id, dir).map(|_| ()));
            let _ = owner.store.forget(&root, dir);
            return Err(e);
        }
        self.owned().insert(dir.clone(), levels);
        Ok(())
    }

    /// Releases a lock obtained through this handle.
    pub fn wc_lock_release(&self, dir: &RelPath) -> Result<()> {
        let owner = self.owner()?;
        let _gate = self.gate();
        if !self.owned().contains_key(dir) {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("working copy not locked at '{dir}'"),
            ));
        }
        // The lock stays ours until the row is really gone, so a failed delete can be retried.
        self.with_write(|tx| delete_row(tx, self.wc_id, dir).map(|_| ()))?;
        self.owned().remove(dir);
        owner.store.forget(&self.canonical_root()?, dir)?;
        self.remove_markers_for(dir);
        Ok(())
    }

    fn remove_markers_for(&self, dir: &RelPath) {
        if let Ok(markers) = self.markers() {
            for (path, marker) in markers {
                if marker.dir == dir.as_str() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
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

    /// Removes the lock row on exactly `dir` (and its ownership record and markers) on the user's
    /// explicit confirmation. Returns whether a row existed.
    pub fn wc_lock_steal(&self, dir: &RelPath) -> Result<bool> {
        let owner = self.owner()?;
        let _gate = self.gate();
        let removed = self.with_write(|tx| delete_row(tx, self.wc_id, dir))?;
        self.owned().remove(dir);
        owner.store.forget(&self.canonical_root()?, dir)?;
        self.remove_markers_for(dir);
        Ok(removed)
    }

    /// Recovers locks of dead application processes. A row is removed only when the ownership
    /// store reports its owner dead **and** a matching marker file proves the row is the one we
    /// left; markers whose row is gone are swept. Everything else (live owners, foreign rows,
    /// rows of unversioned directories) is left alone. All row removals happen in one
    /// transaction.
    pub fn wc_cleanup_locks(&self) -> Result<LockCleanup> {
        let owner = self.owner()?;
        let root = self.canonical_root()?;
        let _gate = self.gate();
        let dead = owner.store.owned_by_dead_process(&root)?;
        let markers = self.markers()?;
        let ours: BTreeSet<RelPath> = self.owned().keys().cloned().collect();

        let mut report = LockCleanup::default();
        let mut to_remove: Vec<(RelPath, i32)> = Vec::new();
        for dir in &dead {
            if ours.contains(dir) {
                continue;
            }
            match markers.iter().find(|(_, m)| m.dir == dir.as_str()) {
                Some((_, m)) => to_remove.push((dir.clone(), m.levels)),
                None => report.needs_user_action.push(dir.clone()),
            }
        }
        let removed = self.with_write(|tx| {
            let mut removed = Vec::new();
            for (dir, levels) in &to_remove {
                // Only the exact row we left: same directory and same levels as the marker says.
                let n = tx
                    .execute(
                        "DELETE FROM wc_lock WHERE wc_id = ?1 AND local_dir_relpath = ?2 \
                         AND locked_levels = ?3",
                        params![self.wc_id, dir.as_str(), levels],
                    )
                    .map_err(sql_err)?;
                if n > 0 {
                    removed.push(dir.clone());
                }
            }
            Ok(removed)
        })?;
        report.recovered = removed;
        // Bookkeeping after the rows are gone; a crash here only leaves things for the next run.
        for (dir, _) in &to_remove {
            owner.store.forget(&root, dir)?;
        }
        for dir in &report.needs_user_action {
            owner.store.forget(&root, dir)?; // the record can no longer prove anything
        }
        let live_rows: Vec<(String, i64)> = self.with_read(|c| {
            let mut stmt = c
                .prepare("SELECT local_dir_relpath, locked_levels FROM wc_lock WHERE wc_id = ?1")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([self.wc_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?
                .collect::<std::result::Result<Vec<(String, i64)>, _>>()
                .map_err(sql_err)?;
            Ok(rows)
        })?;
        for (path, marker) in markers {
            let row_exists = live_rows
                .iter()
                .any(|(d, l)| *d == marker.dir && *l == i64::from(marker.levels));
            let is_ours_alive = RelPath::new(&marker.dir).is_ok_and(|d| ours.contains(&d));
            if !row_exists && !is_ours_alive {
                let _ = std::fs::remove_file(path);
            }
        }
        report.recovered.sort();
        report.needs_user_action.sort();
        Ok(report)
    }
}

/// Adds the cleanup hint users need when a lock blocks them.
fn stale_hint(e: SvnError) -> SvnError {
    if e.code == ErrorCode::WcBusy {
        SvnError::new(
            e.code,
            format!(
                "{}; if no Subversion client is running, run cleanup to remove the stale lock",
                e.message
            ),
        )
    } else {
        e
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
