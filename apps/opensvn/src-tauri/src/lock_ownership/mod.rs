//! [`LockOwnershipStore`] on `app.sqlite` (`wc_lock_owner`): wc.db records no lock owner, so the
//! app remembers which process holds each working copy directory lock and can tell a live
//! holder from a stale row left by a crashed process.

mod alive_probe;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use svn_types::{AliveProbe, ErrorCode, LockOwnershipStore, RelPath, Result, SvnError};

use crate::db::{AppDb, DbError, now_iso};

pub use alive_probe::{OsAliveProbe, current_boot_id};

/// SQLite-backed ownership registry.
pub struct SqliteLockOwnership {
    db: AppDb,
    probe: Arc<dyn AliveProbe>,
}

fn storage(e: DbError) -> SvnError {
    SvnError::new(ErrorCode::Internal, format!("lock ownership store: {e}"))
}

/// Lexical normalization of the root (drops `.` components and trailing separators), stored as text.
fn root_key(wcroot: &Path) -> String {
    wcroot
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

impl SqliteLockOwnership {
    /// Store over `db` using `probe` to judge liveness.
    pub fn new(db: AppDb, probe: Arc<dyn AliveProbe>) -> Self {
        Self { db, probe }
    }

    /// Store with the real operating system probe.
    pub fn with_os_probe(db: AppDb) -> Self {
        Self::new(db, Arc::new(OsAliveProbe))
    }
}

impl LockOwnershipStore for SqliteLockOwnership {
    fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()> {
        let root = root_key(wcroot);
        self.db
            .conn_handle()
            .with(|c| {
                // IMMEDIATE: another app instance must not slip in between the check and the write.
                let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing: Option<(u32, String)> = tx
                    .query_row(
                        "SELECT pid, boot_id FROM wc_lock_owner WHERE wc_root = ?1 AND dir = ?2",
                        params![root, dir.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((old_pid, old_boot)) = existing
                    && (old_pid != pid || old_boot != boot_id)
                    && self.probe.is_alive(old_pid, &old_boot)
                {
                    return Ok(Err(SvnError::new(
                        ErrorCode::WcBusy,
                        format!("{dir} is locked by process {old_pid}"),
                    )));
                }
                tx.execute(
                    "INSERT INTO wc_lock_owner (wc_root, dir, pid, boot_id, acquired_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(wc_root, dir) DO UPDATE SET pid = excluded.pid,
                       boot_id = excluded.boot_id, acquired_at = excluded.acquired_at",
                    params![root, dir.as_str(), pid, boot_id, now_iso()],
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
        let rows: Vec<(String, u32, String)> = self
            .db
            .conn_handle()
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT dir, pid, boot_id FROM wc_lock_owner WHERE wc_root = ?1 ORDER BY dir",
                )?;
                let rows = stmt
                    .query_map(params![root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .map_err(storage)?;
        let mut dead = Vec::new();
        for (dir, pid, boot) in rows {
            if !self.probe.is_alive(pid, &boot) {
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
