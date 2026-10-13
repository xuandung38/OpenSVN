//! Opening a working copy: describe it and repair what a crash left behind.
//!
//! Order, before the first write of the session:
//! 1. Directory locks (`WC_LOCK`) left by a dead process of this app are released through
//!    [`WcDb::wc_cleanup_locks`]: a row goes only when the ownership registry reports its owner
//!    dead **and** the `.svn/tmp/opensvn-lock-*` marker of that exact row (directory and levels)
//!    exists. Every other row (svn CLI, TortoiseSVN, an unknown owner, a record without marker)
//!    stays and is reported for the user to decide.
//! 2. When no foreign lock is left, unfinished work queue items are run.
//! 3. An interrupted commit is cleaned up from its journal: a leaked server transaction is
//!    deleted, a merged commit is finalized in the working copy (only when no foreign lock and no
//!    unfinished work is left).
//!
//! Recovery that needs the working copy layer goes through [`RecoveryHooks`]; a hook that fails
//! leaves the work in place and reports the working copy for the user to clean up.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use svn_types::{ErrorCode, LockOwnershipStore, RepoUrl, SvnError};
use svn_wc::WcDb;
use svn_wc::db::{LockOwner, sql_err};

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
    /// Directories whose stale lock of a dead app process was removed.
    recovered: Vec<String>,
    /// Directories the user has to look at (foreign rows, dead records without a marker).
    needs_user_action: Vec<String>,
    /// A lock row of another program is still in place: nothing may write to the working copy.
    foreign_held: bool,
}

fn add_once(list: &mut Vec<String>, dir: &str) {
    if !list.iter().any(|d| d == dir) {
        list.push(dir.to_owned());
    }
}

/// Releases the stale locks of dead app processes and lists every lock that is left.
fn recover_locks(db: &WcDb, owner: LockOwner) -> EngineResult<LockOutcome> {
    db.set_lock_owner(owner);
    let cleanup = db.wc_cleanup_locks()?;
    // This handle holds no lock of its own, so every row still there belongs to someone else.
    let left: Vec<String> = db.with_read(|c| {
        let mut stmt = c
            .prepare("SELECT local_dir_relpath FROM wc_lock WHERE wc_id = ?1")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([db.wc_id()], |r| r.get::<_, String>(0))
            .map_err(sql_err)?;
        rows.collect::<Result<_, _>>().map_err(sql_err)
    })?;
    let mut needs_user_action = Vec::new();
    for dir in cleanup
        .needs_user_action
        .iter()
        .map(|d| d.as_str())
        .chain(left.iter().map(String::as_str))
    {
        add_once(&mut needs_user_action, dir);
    }
    needs_user_action.sort();
    Ok(LockOutcome {
        recovered: cleanup
            .recovered
            .iter()
            .map(|d| d.as_str().to_owned())
            .collect(),
        needs_user_action,
        foreign_held: !left.is_empty(),
    })
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

    pub(super) fn lock_owner(&self) -> LockOwner {
        LockOwner {
            store: self.lock_ownership.clone() as Arc<dyn LockOwnershipStore>,
            pid: std::process::id(),
            boot_id: self.boot_id.clone(),
        }
    }

    async fn recover(&self, db: &Arc<WcDb>, ctx: &OpContext) -> EngineResult<RecoveryReport> {
        let (handle, owner) = (db.clone(), self.lock_owner());
        let locks = blocking(move || recover_locks(&handle, owner)).await?;
        let mut report = RecoveryReport {
            recovered: locks.recovered,
            needs_user_action: locks.needs_user_action,
        };
        // Work items cannot run while somebody else holds a directory lock.
        let work_left = if locks.foreign_held {
            let handle = db.clone();
            blocking(move || queue_is_pending(&handle)).await?
        } else {
            !self.recover_work_queue(db, &mut report, ctx).await?
        };
        self.recover_commit(db, &mut report, locks.foreign_held || work_left, ctx)
            .await?;
        Ok(report)
    }

    /// Runs unfinished work items; a failure leaves them in place for the user's cleanup.
    /// Returns whether the queue is empty afterwards.
    async fn recover_work_queue(
        &self,
        db: &Arc<WcDb>,
        report: &mut RecoveryReport,
        ctx: &OpContext,
    ) -> EngineResult<bool> {
        let handle = db.clone();
        if !blocking(move || queue_is_pending(&handle)).await? {
            return Ok(true);
        }
        match self.recovery.run_work_queue(db, ctx).await {
            Ok(()) => {
                add_once(&mut report.recovered, ROOT);
                Ok(true)
            }
            Err(e) if e.code == ErrorCode::Cancelled => Err(e),
            Err(e) => {
                tracing::warn!(error = %e, "unfinished work queue items need cleanup");
                add_once(&mut report.needs_user_action, ROOT);
                Ok(false)
            }
        }
    }

    /// Cleans up after an interrupted commit using its journal. `wc_busy` (a foreign lock or
    /// unfinished work items) keeps a merged commit from being finalized.
    async fn recover_commit(
        &self,
        db: &Arc<WcDb>,
        report: &mut RecoveryReport,
        wc_busy: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let wc_root = db.wcroot();
        let record = match self.journals.load(wc_root) {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(()),
            Err(e) => {
                // Keeping an unreadable journal would block every later commit; it is moved
                // aside (kept for diagnosis) and the user is told that a commit was interrupted.
                tracing::warn!(error = %e, "setting an unreadable commit journal aside");
                if let Err(e) = self.journals.quarantine(wc_root) {
                    tracing::warn!(error = %e, "could not set the commit journal aside");
                    self.clear_journal(wc_root);
                }
                add_once(&mut report.needs_user_action, ROOT);
                return Ok(());
            }
        };
        match &record.phase {
            JournalPhase::Started => self.clear_journal(wc_root),
            JournalPhase::TxnCreated { txn } => match self.abort_txn(&record, txn, ctx).await {
                Ok(()) => self.clear_journal(wc_root),
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) if e.code == ErrorCode::PathNotFound => {
                    // The server no longer knows the transaction: it expired, was removed, or
                    // was merged right before the crash. Retrying can never succeed, so the
                    // journal goes; whether the commit landed is for the user to check.
                    tracing::warn!(error = %e, txn, "the abandoned transaction is gone");
                    self.clear_journal(wc_root);
                    add_once(&mut report.needs_user_action, ROOT);
                }
                Err(e) => {
                    // Network or sign-in problems are temporary: the journal stays, the next
                    // open tries again. The working copy itself is untouched.
                    tracing::warn!(error = %e, txn, "could not delete the abandoned transaction");
                }
            },
            JournalPhase::Merged { rev } if wc_busy => {
                tracing::warn!(rev, "merged commit waits until the working copy is free");
                add_once(&mut report.needs_user_action, ROOT);
            }
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
        // Sign in first, so a prompt never happens in the middle of the delete.
        session.ensure_authenticated(&ctx.cancel).await?;
        session.abort_txn(txn).await
    }

    fn clear_journal(&self, wc_root: &Path) {
        if let Err(e) = self.journals.clear(wc_root) {
            tracing::warn!(error = %e, "could not remove the commit journal");
        }
    }
}
