//! [`LockOwnershipStore`] on `app.sqlite` (`wc_lock_owner`): wc.db records no lock owner, so the
//! app remembers which process holds each working copy directory lock and can tell a live
//! holder from a stale row left by a crashed process.
//!
//! A row stores the owner's pid, the boot session and the process start time. Classification:
//! * the recorded boot id is exact (Linux `boot_id`, macOS boot session UUID) and differs from the
//!   current one: the machine rebooted, **dead**;
//! * no live process with that pid: **dead**;
//! * the pid is alive but its start stamp differs from the recorded one: the pid was recycled,
//!   **dead** (on Linux the stamp is ticks since boot, so a clock change never moves it);
//! * the pid is alive with a matching start time and the same boot session: **alive**;
//! * the pid is alive with a matching start time but the boot id differs (a clock change moved a
//!   derived boot time): **unknown**. Unknown owners are never taken over automatically.
//!
//! Rows written before the Linux start stamp became ticks since boot hold wall-clock seconds and
//! classify as **dead** on Linux (see "Upgrade notes" in `docs/deployment-guide.md`).
//!
//! Callers pass a canonical `wcroot` (see [`LockOwnershipStore`]); this store also canonicalizes
//! when the path exists and removes the Windows verbatim prefix so one folder has one key.

mod alive_probe;
mod open_files;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use svn_types::{ErrorCode, LockOwnershipStore, RelPath, Result, SvnError};

use crate::db::{AppDb, DbError, now_iso};

pub use alive_probe::{OsProcessProbe, ProcessProbe, current_boot_id};
pub use open_files::{OpenBy, other_processes_with_open};

/// State of a recorded lock owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Liveness {
    Alive,
    Dead,
    Unknown,
}

/// SQLite-backed ownership registry.
pub struct SqliteLockOwnership {
    db: AppDb,
    probe: Arc<dyn ProcessProbe>,
}

fn storage(e: DbError) -> SvnError {
    SvnError::new(ErrorCode::Internal, format!("lock ownership store: {e}"))
}

/// Stable key of a working copy root: canonical when the folder exists, otherwise the lexically
/// cleaned path; the Windows `\\?\` prefix is dropped and drive paths are lower-cased (NTFS is
/// case-insensitive). Unverified on Windows: no runner is available.
fn root_key(wcroot: &Path) -> String {
    let resolved =
        std::fs::canonicalize(wcroot).unwrap_or_else(|_| wcroot.components().collect::<PathBuf>());
    let text = resolved.to_string_lossy().into_owned();
    if cfg!(windows) {
        let trimmed = text.strip_prefix(r"\\?\").unwrap_or(&text);
        trimmed.to_lowercase()
    } else {
        text
    }
}

impl SqliteLockOwnership {
    /// Store over `db` using `probe` to judge liveness.
    pub fn new(db: AppDb, probe: Arc<dyn ProcessProbe>) -> Self {
        Self { db, probe }
    }

    /// Store with the real operating system probe.
    pub fn with_os_probe(db: AppDb) -> Self {
        Self::new(db, Arc::new(OsProcessProbe))
    }

    fn classify(&self, pid: u32, boot_id: &str, recorded_start: Option<u64>) -> Liveness {
        if self.probe.rebooted_since(boot_id) {
            return Liveness::Dead; // nothing survives a reboot, whoever has the pid now
        }
        let Some(now_start) = self.probe.start_time(pid) else {
            return Liveness::Dead;
        };
        if recorded_start.is_some_and(|s| !self.probe.same_process(s, now_start)) {
            return Liveness::Dead; // the pid now belongs to a different process
        }
        if self.probe.same_boot(boot_id) {
            Liveness::Alive
        } else {
            Liveness::Unknown
        }
    }
}

impl LockOwnershipStore for SqliteLockOwnership {
    fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()> {
        let root = root_key(wcroot);
        let start_time = self.probe.start_time(pid);
        let start_sql = start_time.and_then(|s| i64::try_from(s).ok());
        self.db
            .conn_handle()
            .with(|c| {
                // IMMEDIATE: another app instance must not slip in between the check and the write.
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing: Option<(u32, String, Option<i64>)> = tx
                    .query_row(
                        "SELECT pid, boot_id, start_time FROM wc_lock_owner
                         WHERE wc_root = ?1 AND dir = ?2",
                        params![root, dir.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((old_pid, old_boot, old_start)) = existing
                    && (old_pid != pid || old_boot != boot_id)
                {
                    let old_start = old_start.and_then(|s| u64::try_from(s).ok());
                    match self.classify(old_pid, &old_boot, old_start) {
                        Liveness::Dead => {}
                        Liveness::Alive => {
                            return Ok(Err(SvnError::new(
                                ErrorCode::WcBusy,
                                format!("{dir} is locked by process {old_pid}"),
                            )));
                        }
                        Liveness::Unknown => {
                            return Ok(Err(SvnError::new(
                                ErrorCode::WcBusy,
                                format!(
                                    "{dir} is locked by process {old_pid}, which may still be running"
                                ),
                            )));
                        }
                    }
                }
                tx.execute(
                    "INSERT INTO wc_lock_owner (wc_root, dir, pid, boot_id, start_time, acquired_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(wc_root, dir) DO UPDATE SET pid = excluded.pid,
                       boot_id = excluded.boot_id, start_time = excluded.start_time,
                       acquired_at = excluded.acquired_at",
                    params![root, dir.as_str(), pid, boot_id, start_sql, now_iso()],
                )?;
                tx.commit()?;
                Ok(Ok(()))
            })
            .map_err(storage)?
    }

    fn forget(&self, wcroot: &Path, dir: &RelPath) -> Result<()> {
        let root = root_key(wcroot);
        self.db
            .conn_handle()
            .with(|c| {
                c.execute(
                    "DELETE FROM wc_lock_owner WHERE wc_root = ?1 AND dir = ?2",
                    params![root, dir.as_str()],
                )?;
                Ok(())
            })
            .map_err(storage)
    }

    fn owned_by_dead_process(&self, wcroot: &Path) -> Result<Vec<RelPath>> {
        let root = root_key(wcroot);
        let rows: Vec<(String, u32, String, Option<i64>)> = self
            .db
            .conn_handle()
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT dir, pid, boot_id, start_time FROM wc_lock_owner
                     WHERE wc_root = ?1 ORDER BY dir",
                )?;
                let rows = stmt
                    .query_map(params![root], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .map_err(storage)?;
        let mut dead = Vec::new();
        for (dir, pid, boot, start) in rows {
            // Only provably dead owners; "unknown" stays for the user to decide.
            let start = start.and_then(|s| u64::try_from(s).ok());
            if self.classify(pid, &boot, start) == Liveness::Dead {
                dead.push(RelPath::new(&dir)?);
            }
        }
        dead.sort();
        Ok(dead)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
