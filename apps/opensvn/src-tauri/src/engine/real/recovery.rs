//! Opening a working copy: describe it and repair what a crash left behind.
//!
//! Order, before the first write of the session:
//! 1. Directory locks (`WC_LOCK`) left by a dead process of this app are released through
//!    [`WcDb::wc_cleanup_locks`]: a row goes only when the ownership registry reports its owner
//!    dead **and** the `.svn/tmp/opensvn-lock-*` marker of that exact row (directory and levels)
//!    exists. Every other row (svn CLI, TortoiseSVN, an unknown owner, a record without marker)
//!    stays and is reported for the user to decide.
//! 2. When no foreign lock is left, unfinished work queue items are run under the working copy
//!    lock ([`RecoveryHooks::run_work_queue`]; [`ClientRecovery`] in the app).
//! 3. Interrupted commits of this working copy are settled from the `svn-client` commit
//!    journals below `<state_dir>/commit-journals/` ([`svn_client::commit::recover_wc_commits`])
//!    over an unattended session (opening never prompts): a merged commit is finalized, a leaked
//!    transaction deleted, a commit whose `MERGE` outcome is unknown looked up in the log.
//!    Whatever cannot be settled stays on disk and is reported for the user. Nothing is done
//!    while a foreign lock or unfinished work is left. The repository part (session and
//!    recovery) is bounded by [`RECOVERY_NETWORK_TIMEOUT`]: a server that does not answer cannot
//!    hold the working copy's write slot longer; the journals stay and are retried on the next
//!    open. Journal files are attributed to working copies by the owner hint in their names
//!    ([`JournalStore::may_belong_to`]), so a file that cannot be read is reported (and blocks
//!    commits) only for the working copy it may belong to.
//!
//! The commit journal of earlier app builds (`<state_dir>/commit-journal/<sha1>.json`) is no
//! longer read: a file left for this working copy is moved aside to `<file>.legacy` (never
//! deleted) and the working copy is reported for the user, who checks the repository log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sha1::{Digest, Sha1};
use svn_client::commit::WcCommitRecovery;
use svn_client::commit::journal::{JournalLoadError, JournalStore, move_no_replace};
use svn_ra_api::RaSession;
use svn_types::{ErrorCode, LockOwnershipStore, RepoUrl};
use svn_wc::WcDb;
use svn_wc::db::{LockOwner, find_wcroot, sql_err};

use super::wc_read::{blocking, describe, open_db_canonical};
use super::{ConnectOptions, RealEngine};
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, RecoveryReport, WcOpened};

/// Directory below the state directory that holds the `svn-client` commit journals.
const JOURNAL_DIR: &str = "commit-journals";
/// Directory of the commit journal written by earlier app builds.
const LEGACY_JOURNAL_DIR: &str = "commit-journal";
/// How many set-aside copies of one legacy journal may exist.
const LEGACY_SLOTS: usize = 8;
/// Longest time opening a working copy spends on the repository for commit recovery (connecting
/// included). Network calls are only awaited between journal writes and working copy updates
/// that are each complete on their own, so stopping there leaves every journal as it was or
/// settled; what is left is reported and retried on the next open.
pub(super) const RECOVERY_NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// Steps of crash recovery that need the working copy layer.
#[async_trait]
pub trait RecoveryHooks: Send + Sync {
    /// Runs the unfinished work queue items of `db` (whose lock owner is set). `Ok` means the
    /// queue is empty afterwards.
    async fn run_work_queue(&self, db: &Arc<WcDb>, ctx: &OpContext) -> EngineResult<()>;
}

/// Recovery through `svn-client`: the work queue runs under the working copy write lock.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClientRecovery;

#[async_trait]
impl RecoveryHooks for ClientRecovery {
    async fn run_work_queue(&self, db: &Arc<WcDb>, ctx: &OpContext) -> EngineResult<()> {
        let (db, cancel) = (db.clone(), ctx.cancel.clone());
        blocking(move || {
            svn_client::local::lock::with_wc_lock(&db, &cancel, || {
                svn_client::wc::run_work_queue(&db, &cancel).map(|_| ())
            })
        })
        .await
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

/// What the commit journals hold for one working copy.
#[derive(Debug, Default, PartialEq, Eq)]
struct JournalScan {
    /// A journal of this working copy, or a file whose working copy cannot be known (unreadable,
    /// corrupt, unknown format without a root): recovery has something to do or to report.
    relevant: bool,
    /// One of them may need the repository (no revision recorded, or not readable here).
    needs_session: bool,
}

/// Looks at the journals in `dir` without changing anything.
fn scan_journals(dir: &Path, wcroot: &Path) -> EngineResult<JournalScan> {
    let store = JournalStore::new(dir);
    let mut scan = JournalScan::default();
    for path in store.list()? {
        // Another working copy's file (by the owner hint in its name), readable or not.
        if !JournalStore::may_belong_to(&path, wcroot) {
            continue;
        }
        match JournalStore::load(&path) {
            Ok(data) if data.wcroot == wcroot => {
                scan.relevant = true;
                scan.needs_session |= data.merged.is_none();
            }
            Ok(_) => {}
            Err(JournalLoadError::UnsupportedVersion {
                wcroot: Some(other),
                ..
            }) if other != wcroot => {}
            Err(_) => scan.relevant = true,
        }
    }
    Ok(scan)
}

/// Path of the legacy journal file of the working copy at `wcroot` (the name earlier builds
/// derived from the canonical root path).
fn legacy_journal_file(state_dir: &Path, wcroot: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(wcroot).unwrap_or_else(|_| wcroot.to_path_buf());
    let digest = Sha1::digest(canonical.to_string_lossy().as_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    state_dir
        .join(LEGACY_JOURNAL_DIR)
        .join(format!("{name}.json"))
}

/// Sets the legacy journal `file` aside under the first free name of `<file>.legacy`,
/// `<file>.legacy.1`, ... Returns the new name; on failure the file stays where it is.
fn set_legacy_aside(file: &Path) -> std::io::Result<PathBuf> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 0..LEGACY_SLOTS {
        let aside = match n {
            0 => file.with_file_name(format!("{name}.legacy")),
            n => file.with_file_name(format!("{name}.legacy.{n}")),
        };
        match move_no_replace(file, &aside) {
            Ok(()) => return Ok(aside),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free name to set the legacy commit journal aside",
    ))
}

/// Folds what commit recovery did into the open report: a finalized commit is a recovery; a
/// journal that stays (outcome unknown, or not processable now) needs the user.
fn apply_commit_recovery(outcome: &WcCommitRecovery, report: &mut RecoveryReport) {
    for id in &outcome.aborted {
        tracing::info!(journal = %id, "interrupted commit cleaned up; it never became a revision");
    }
    for id in &outcome.finalized {
        tracing::info!(journal = %id, "interrupted commit finalized in the working copy");
    }
    if !outcome.finalized.is_empty() {
        add_once(&mut report.recovered, ROOT);
    }
    for (id, e) in &outcome.needs_user {
        tracing::warn!(journal = %id, error = %e, "interrupted commit needs the user");
    }
    for (id, e) in &outcome.failed {
        tracing::warn!(journal = %id, error = %e, "interrupted commit not settled yet; kept");
    }
    if !outcome.needs_user.is_empty() || !outcome.failed.is_empty() {
        add_once(&mut report.needs_user_action, ROOT);
    }
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
            // Journals are keyed by the canonical root (as commits open the working copy); the
            // description keeps the root spelled as the caller knows it.
            let shown = find_wcroot(&root)?;
            let db = open_db_canonical(&root)?;
            let mut info = describe(&db)?;
            info.root_path = shown.to_string_lossy().into_owned();
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
        let recovery = self.recover(&db, &info.repo_root_url, ctx).await?;
        Ok(WcOpened { info, recovery })
    }

    pub(super) fn lock_owner(&self) -> LockOwner {
        LockOwner {
            store: self.lock_ownership.clone() as Arc<dyn LockOwnershipStore>,
            pid: std::process::id(),
            boot_id: self.boot_id.clone(),
        }
    }

    /// Directory of the `svn-client` commit journals: the `CommitContext::state_dir` of commits
    /// and the directory commit recovery reads.
    pub fn commit_journal_dir(&self) -> PathBuf {
        self.state_dir.join(JOURNAL_DIR)
    }

    async fn recover(
        &self,
        db: &Arc<WcDb>,
        repo_root_url: &str,
        ctx: &OpContext,
    ) -> EngineResult<RecoveryReport> {
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
        self.migrate_legacy_journal(db.wcroot(), &mut report);
        self.recover_commits(
            db,
            repo_root_url,
            &mut report,
            locks.foreign_held || work_left,
            ctx,
        )
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

    /// Sets the journal earlier builds left for this working copy aside (never deleted): this
    /// build cannot tell from it whether its commit was applied, so the user is asked to check.
    fn migrate_legacy_journal(&self, wcroot: &Path, report: &mut RecoveryReport) {
        let file = legacy_journal_file(&self.state_dir, wcroot);
        if std::fs::symlink_metadata(&file).is_err() {
            return;
        }
        match set_legacy_aside(&file) {
            Ok(aside) => tracing::warn!(
                aside = %aside.display(),
                "commit journal of an earlier app version set aside; check the repository log \
                 for the interrupted commit"
            ),
            Err(e) => tracing::error!(
                error = %e,
                file = %file.display(),
                "commit journal of an earlier app version could not be set aside; left in place"
            ),
        }
        add_once(&mut report.needs_user_action, ROOT);
    }

    /// Settles the interrupted commits of this working copy from their journals. `wc_busy` (a
    /// foreign lock or unfinished work items) keeps everything in place for the next open.
    async fn recover_commits(
        &self,
        db: &Arc<WcDb>,
        repo_root_url: &str,
        report: &mut RecoveryReport,
        wc_busy: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        let dir = self.commit_journal_dir();
        let scan = {
            let (dir, wcroot) = (dir.clone(), db.wcroot().to_path_buf());
            blocking(move || scan_journals(&dir, &wcroot)).await
        };
        let scan = match scan {
            Ok(scan) => scan,
            Err(e) => {
                tracing::warn!(error = %e, "commit journals could not be listed");
                add_once(&mut report.needs_user_action, ROOT);
                return Ok(());
            }
        };
        if !scan.relevant {
            return Ok(());
        }
        if wc_busy {
            tracing::warn!("interrupted commit waits until the working copy is free");
            add_once(&mut report.needs_user_action, ROOT);
            return Ok(());
        }
        // Opening never prompts: without a usable session only what needs no server is done,
        // the rest is reported and retried on the next open.
        let run = async {
            let session = if scan.needs_session {
                match self.unattended_session(repo_root_url, ctx).await {
                    Ok(s) => Some(s),
                    Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                    Err(e) => {
                        tracing::warn!(error = %e, "no repository session for commit recovery");
                        None
                    }
                }
            } else {
                None
            };
            Ok(
                svn_client::commit::recover_wc_commits(&dir, db, session.as_deref(), &ctx.cancel)
                    .await,
            )
        };
        let outcome = tokio::time::timeout(self.recovery_network_timeout, run).await;
        ctx.check()?;
        match outcome {
            Ok(Ok(Ok(outcome))) => apply_commit_recovery(&outcome, report),
            Ok(Err(e)) => return Err(e),
            Ok(Ok(Err(e))) => {
                tracing::warn!(error = %e, "commit recovery failed");
                add_once(&mut report.needs_user_action, ROOT);
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = self.recovery_network_timeout.as_secs(),
                    "commit recovery stopped: the repository did not answer in time; retried on \
                     the next open"
                );
                add_once(&mut report.needs_user_action, ROOT);
            }
        }
        Ok(())
    }

    /// Session at the repository root that fails with `auth-required` instead of prompting.
    pub(super) async fn unattended_session(
        &self,
        repo_root_url: &str,
        ctx: &OpContext,
    ) -> EngineResult<Arc<dyn RaSession>> {
        let url = RepoUrl::parse(repo_root_url)?;
        let options = ConnectOptions {
            interactive: false,
            ..ConnectOptions::root(ctx)
        };
        self.connector
            .connect_with(&url, &options, &ctx.cancel)
            .await
    }
}
