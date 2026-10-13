//! Opening a working copy: describe it and repair what a crash left behind.
//!
//! Order, before the first write of the session:
//! 1. Directory locks (`WC_LOCK`) held by a process that is gone are released. Only locks that the
//!    ownership registry knows to belong to a dead process are touched; any other lock (another
//!    program, an unknown owner, a registry that lost its row) is reported for the user to decide.
//! 2. When nothing else holds a lock, unfinished work queue items are run.
//! 3. An interrupted commit is cleaned up from its journal: a leaked server transaction is
//!    deleted, a merged commit is finalized in the working copy.
//!
//! Recovery that needs the working copy layer goes through [`RecoveryHooks`]; a hook that fails
//! leaves the work in place and reports the working copy for the user to clean up.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use svn_types::{ErrorCode, RelPath, RepoUrl, SvnError};
use svn_wc::WcDb;
use svn_wc::db::sql_err;

use super::RealEngine;
use super::journal::{JournalPhase, JournalRecord};
use super::wc_read::{blocking, describe, open_db};
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, RecoveryReport, WcOpened};

/// Steps of crash recovery that need the working copy layer (`svn-client`).
#[async_trait]
pub trait RecoveryHooks: Send + Sync {
    /// Runs the unfinished work queue items of `db`. `Ok` means the queue is empty afterwards.
    async fn run_work_queue(&self, db: &WcDb, ctx: &OpContext) -> EngineResult<()>;

    /// Brings the working copy up to date for a commit the server merged as `record`'s revision.
    async fn finalize_commit(
        &self,
        db: &WcDb,
        record: &JournalRecord,
        ctx: &OpContext,
    ) -> EngineResult<()>;
}

/// Hooks used until the working copy layer is wired: both steps fail, so the working copy is
/// reported for cleanup and its work stays on disk untouched.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnwiredRecovery;

#[async_trait]
impl RecoveryHooks for UnwiredRecovery {
    async fn run_work_queue(&self, _db: &WcDb, _ctx: &OpContext) -> EngineResult<()> {
        Err(SvnError::new(
            ErrorCode::Internal,
            "not wired: running the work queue needs the svn-client working copy operations",
        ))
    }

    async fn finalize_commit(
        &self,
        _db: &WcDb,
        _record: &JournalRecord,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(SvnError::new(
            ErrorCode::Internal,
            "not wired: finalizing a commit needs the svn-client working copy operations",
        ))
    }
}

/// Directories reported for the working copy root itself.
const ROOT: &str = "";

/// What the lock step found.
struct LockOutcome {
    recovered: Vec<String>,
    foreign: Vec<String>,
}

fn add_once(list: &mut Vec<String>, dir: &str) {
    if !list.iter().any(|d| d == dir) {
        list.push(dir.to_owned());
    }
}

/// Releases dead-owner locks and lists the rest.
fn recover_locks(
    db: &WcDb,
    store: &dyn svn_types::LockOwnershipStore,
) -> EngineResult<LockOutcome> {
    let wc_id = db.wc_id();
    let held: BTreeSet<String> = db.with_read(|c| {
        let mut stmt = c
            .prepare("SELECT local_dir_relpath FROM wc_lock WHERE wc_id = ?1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([wc_id], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    })?;
    let dead: Vec<RelPath> = store.owned_by_dead_process(db.wcroot())?;
    let (recovered, foreign): (Vec<String>, Vec<String>) = held
        .into_iter()
        .partition(|dir| dead.iter().any(|d| d.as_str() == dir));
    if !recovered.is_empty() {
        db.with_write(|tx| {
            for dir in &recovered {
                tx.execute(
                    "DELETE FROM wc_lock WHERE wc_id = ?1 AND local_dir_relpath = ?2",
                    (wc_id, dir),
                )
                .map_err(sql_err)?;
            }
            Ok(())
        })?;
    }
    // Forget every dead record, also those without a row (the lock was already released): the
    // registry must not keep describing locks that no longer exist.
    for dir in &dead {
        store.forget(db.wcroot(), dir)?;
    }
    Ok(LockOutcome { recovered, foreign })
}

fn queue_is_pending(db: &WcDb) -> EngineResult<bool> {
    db.with_read(|c| {
        c.query_row("SELECT EXISTS (SELECT 1 FROM work_queue)", [], |r| {
            r.get::<_, bool>(0)
        })
        .map_err(sql_err)
    })
}

impl RealEngine {
    pub(super) async fn open_wc_with_recovery(
        &self,
        root: &Path,
        ctx: &OpContext,
    ) -> EngineResult<WcOpened> {
        ctx.check()?;
        let root = root.to_path_buf();
        let (db, info) = blocking(move || {
            let db = open_db(&root)?;
            let info = describe(&db)?;
            Ok((Arc::new(db), info))
        })
        .await?;
        if !db.is_writable() {
            // A read-only format cannot hold lock rows or work items we could repair.
            return Ok(WcOpened {
                info,
                recovery: RecoveryReport::default(),
            });
        }
        let recovery = self.recover(&db, ctx).await?;
        Ok(WcOpened { info, recovery })
    }

    async fn recover(&self, db: &Arc<WcDb>, ctx: &OpContext) -> EngineResult<RecoveryReport> {
        let (handle, store) = (db.clone(), self.lock_ownership.clone());
        let locks = blocking(move || recover_locks(&handle, store.as_ref())).await?;
        let mut report = RecoveryReport {
            recovered: locks.recovered,
            needs_user_action: locks.foreign,
        };
        // Work items cannot run while somebody else holds a directory lock.
        if report.needs_user_action.is_empty() {
            self.recover_work_queue(db, &mut report, ctx).await?;
        }
        self.recover_commit(db, &mut report, ctx).await?;
        Ok(report)
    }

    /// Runs unfinished work items; a failure leaves them in place for the user's cleanup.
    async fn recover_work_queue(
        &self,
        db: &Arc<WcDb>,
        report: &mut RecoveryReport,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let handle = db.clone();
        if !blocking(move || queue_is_pending(&handle)).await? {
            return Ok(());
        }
        match self.recovery.run_work_queue(db, ctx).await {
            Ok(()) => add_once(&mut report.recovered, ROOT),
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
            Err(e) => {
                tracing::warn!(error = %e, "unfinished work queue items need cleanup");
                add_once(&mut report.needs_user_action, ROOT);
            }
        }
        Ok(())
    }

    /// Cleans up after an interrupted commit using its journal.
    async fn recover_commit(
        &self,
        db: &Arc<WcDb>,
        report: &mut RecoveryReport,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let wc_root = db.wcroot();
        let record = match self.journals.load(wc_root) {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(()),
            Err(e) => {
                // Keeping an unreadable journal would block every later commit, and the only
                // thing it could still name is a server transaction the server expires itself.
                tracing::warn!(error = %e, "discarding an unreadable commit journal");
                self.clear_journal(wc_root);
                return Ok(());
            }
        };
        match &record.phase {
            JournalPhase::Started => self.clear_journal(wc_root),
            JournalPhase::TxnCreated { txn } => match self.abort_txn(&record, txn, ctx).await {
                Ok(()) => self.clear_journal(wc_root),
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) => {
                    // Network or sign-in problems are temporary: the journal stays, the next
                    // open tries again. The working copy itself is untouched.
                    tracing::warn!(error = %e, txn, "could not delete the abandoned transaction");
                }
            },
            JournalPhase::Merged { rev } => {
                match self.recovery.finalize_commit(db, &record, ctx).await {
                    Ok(()) => {
                        self.clear_journal(wc_root);
                        add_once(&mut report.recovered, ROOT);
                    }
                    Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                    Err(e) => {
                        tracing::warn!(error = %e, rev, "merged commit is not finalized yet");
                        add_once(&mut report.needs_user_action, ROOT);
                    }
                }
            }
        }
        Ok(())
    }

    async fn abort_txn(
        &self,
        record: &JournalRecord,
        txn: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let url = RepoUrl::parse(&record.repo_root_url)?;
        let session = self.connector.connect(&url, &ctx.cancel).await?;
        session.abort_txn(txn).await
    }

    fn clear_journal(&self, wc_root: &Path) {
        if let Err(e) = self.journals.clear(wc_root) {
            tracing::warn!(error = %e, "could not remove the commit journal");
        }
    }
}
