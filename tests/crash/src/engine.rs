//! The OpenSVN engine (`svn-ra` over HTTP + `svn-client` + `svn-wc`) as the app drives it, for
//! one process: a lock owner backed by [`FileLockOwnership`], an HTTP transport without
//! credentials (the test server is anonymous), and the crash recovery the app runs when it
//! opens a working copy (stale locks of dead processes, unfinished work queue, commit journals).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use svn_client::commit::{CommitContext, CommitOptions};
use svn_client::local::revert::RevertOptions;
use svn_client::{ClientCtx, StatusOptions};
use svn_ra::ops::HttpRaSession;
use svn_ra::transport::{HttpTransport, MemoryTrustStore, ProxyConfig, TransportConfig};
use svn_ra_api::{
    AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials, LogRequest, RaSession,
};
use svn_types::{CancelToken, Depth, RepoUrl, Result, Revnum, WcStatus};
use svn_wc::WcDb;
use svn_wc::db::LockOwner;

use crate::owner::{FileLockOwnership, boot_id};

/// Never asks: the test server needs no credentials and serves plain HTTP.
struct NoPrompt;

#[async_trait::async_trait]
impl AuthPrompter for NoPrompt {
    async fn credentials(&self, _req: &CredentialRequest) -> Option<Credentials> {
        None
    }
    async fn confirm_certificate(&self, _info: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

/// What [`Engine::recover`] did.
#[derive(Debug, Default, Clone)]
pub struct Recovered {
    /// Stale lock rows of dead processes that were removed.
    pub locks_removed: usize,
    /// Lock rows left for the user (foreign or without marker).
    pub locks_need_user: usize,
    /// Work queue items that were completed.
    pub work_items: usize,
    /// Commit journals finalized into the working copy.
    pub commits_finalized: usize,
    /// Commit journals dropped because the commit never happened.
    pub commits_aborted: usize,
    /// Commit journals that recovery could not settle (kept for the user), with the reason.
    pub commits_unsettled: Vec<String>,
}

/// One engine instance (one per process, like the app).
pub struct Engine {
    ctx: ClientCtx,
    state_dir: PathBuf,
    transport: Arc<HttpTransport>,
}

impl Engine {
    /// Engine whose lock registry and commit journals live in `state_dir` (shared by all
    /// processes of a test so a later process can recover what a killed one left).
    pub fn new(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| svn_types::SvnError::from_io("state dir", &e))?;
        let owner = LockOwner {
            store: Arc::new(FileLockOwnership::new(state_dir.join("lock-owners.tsv"))),
            pid: std::process::id(),
            boot_id: boot_id(),
        };
        let config = TransportConfig {
            proxy: ProxyConfig::Disabled,
            ..TransportConfig::default()
        };
        let transport = Arc::new(HttpTransport::new(
            config,
            Arc::new(NoPrompt),
            Arc::new(MemoryTrustStore::new()),
        ));
        Ok(Self {
            ctx: ClientCtx::new(owner),
            state_dir: state_dir.to_path_buf(),
            transport,
        })
    }

    /// Opens a session at `url`.
    pub async fn session(&self, url: &str) -> Result<HttpRaSession> {
        HttpRaSession::open(
            Arc::clone(&self.transport),
            RepoUrl::parse(url)?,
            &CancelToken::new(),
        )
        .await
    }

    /// Opens the working copy database with this process as lock owner.
    pub fn open(&self, wc: &Path) -> Result<WcDb> {
        let db = WcDb::open(wc)?;
        db.set_lock_owner(self.ctx.lock_owner.clone());
        Ok(db)
    }

    /// `svn checkout [-r rev] url wc` (HEAD when `rev` is `None`).
    pub async fn checkout(
        &self,
        url: &str,
        wc: &Path,
        rev: Option<Revnum>,
    ) -> Result<Option<Revnum>> {
        let s = self.session(url).await?;
        let sum = svn_client::checkout(
            &self.ctx,
            &s,
            wc,
            rev,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await?;
        Ok(sum.revision)
    }

    /// `svn update wc` to HEAD (or `rev`); `url` is the working copy's URL.
    pub async fn update(
        &self,
        url: &str,
        wc: &Path,
        rev: Option<Revnum>,
    ) -> Result<Option<Revnum>> {
        let s = self.session(url).await?;
        let sum =
            svn_client::update(&self.ctx, &s, wc, rev, Depth::Unknown, &CancelToken::new()).await?;
        Ok(sum.revision)
    }

    /// `svn switch target wc/rel` to HEAD (a subtree switch, like `svn switch ^/branch wc/dir`),
    /// over a session at `url`, the working copy root's URL (the drive is anchored there).
    pub async fn switch(
        &self,
        url: &str,
        wc: &Path,
        rel: &str,
        target: &str,
    ) -> Result<Option<Revnum>> {
        let s = self.session(url).await?;
        let sum = svn_client::switch(
            &self.ctx,
            &s,
            &wc.join(rel),
            &RepoUrl::parse(target)?,
            None,
            Depth::Unknown,
            // The branch is a copy of the switched directory: the ancestry check passes.
            false,
            &CancelToken::new(),
        )
        .await?;
        Ok(sum.revision)
    }

    /// The app's "Continue checkout": after a restart (stale locks of the dead process
    /// recovered), an update of the whole working copy to the revision its incomplete folders
    /// record. `None` when nothing is incomplete.
    pub async fn resume(&self, url: &str, wc: &Path) -> Result<Option<Revnum>> {
        let r = self.recover(url, wc).await?;
        if r.locks_need_user > 0 {
            return Err(svn_types::SvnError::new(
                svn_types::ErrorCode::WcBusy,
                "a lock of unknown owner is in place",
            ));
        }
        let db = self.open(wc)?;
        let rev: Option<i64> = db.with_read(|c| {
            c.query_row(
                "SELECT MAX(revision) FROM nodes WHERE op_depth = 0 AND presence = 'incomplete'",
                [],
                |r| r.get(0),
            )
            .map_err(svn_wc::db::sql_err)
        })?;
        drop(db);
        let Some(rev) = rev else {
            return Ok(None);
        };
        let rev = Revnum(u64::try_from(rev).unwrap_or_default());
        self.update(url, wc, Some(rev)).await
    }

    /// `svn commit wc -m msg`; `None` when there was nothing to commit.
    pub async fn commit(&self, url: &str, wc: &Path, msg: &str) -> Result<Option<Revnum>> {
        let s = self.session(url).await?;
        let db = self.open(wc)?;
        let cancel = CancelToken::new();
        let cctx = CommitContext {
            ra: &s,
            state_dir: &self.journal_dir(),
            cancel: &cancel,
        };
        let r =
            svn_client::commit::commit(&db, &cctx, &[wc.to_path_buf()], &CommitOptions::new(msg))
                .await?;
        Ok(r.map(|r| r.revision))
    }

    /// `svn log [-v] [-l limit] url/path` (newest first, HEAD to r0): returns how many entries
    /// and changed paths the server sent.
    pub async fn log(
        &self,
        url: &str,
        path: &str,
        limit: Option<u32>,
        verbose: bool,
    ) -> Result<(usize, usize)> {
        let s = self.session(url).await?;
        let paths = if path.is_empty() {
            vec![svn_types::RelPath::root()]
        } else {
            vec![svn_types::RelPath::new(path)?]
        };
        let req = LogRequest {
            paths,
            start: None,
            end: None,
            limit,
            discover_changed_paths: verbose,
            strict_node_history: false,
        };
        let (mut entries, mut changed) = (0, 0);
        s.log(
            &req,
            &mut |e| {
                entries += 1;
                changed += e.changed_paths.len();
                Ok(())
            },
            &CancelToken::new(),
        )
        .await?;
        Ok((entries, changed))
    }

    /// `svn revert -R wc`; returns how many paths were reverted.
    pub fn revert(&self, wc: &Path) -> Result<usize> {
        let db = self.open(wc)?;
        let r = svn_client::local::revert::revert(
            &db,
            &CancelToken::new(),
            &[wc.to_path_buf()],
            &RevertOptions { recursive: true },
        )?;
        Ok(r.len())
    }

    /// `svn cleanup wc`.
    pub fn cleanup(&self, wc: &Path) -> Result<usize> {
        let db = self.open(wc)?;
        Ok(svn_client::local::cleanup::cleanup(&db, &CancelToken::new())?.work_items)
    }

    /// Local status of the whole working copy (unversioned items included, unmodified not).
    pub async fn status(&self, wc: &Path) -> Result<Vec<WcStatus>> {
        svn_client::status(
            &self.ctx,
            wc,
            &StatusOptions::default(),
            &CancelToken::new(),
        )
        .await
    }

    fn journal_dir(&self) -> PathBuf {
        self.state_dir.join("commit-journals")
    }

    /// What the app does when it opens a working copy after a crash (see the app's
    /// `engine/real/recovery.rs`): remove stale locks of dead processes, run the unfinished work
    /// queue under the write lock, then settle this working copy's commit journals over a
    /// session at `url`.
    pub async fn recover(&self, url: &str, wc: &Path) -> Result<Recovered> {
        let db = self.open(wc)?;
        let mut out = Recovered::default();
        let locks = db.wc_cleanup_locks()?;
        out.locks_removed = locks.recovered.len();
        out.locks_need_user = locks.needs_user_action.len();
        let cancel = CancelToken::new();
        if out.locks_need_user > 0 {
            return Ok(out); // nothing may write while a lock of unknown owner is in place
        }
        let ran = svn_client::local::lock::with_wc_lock(&db, &cancel, || {
            let n = svn_client::wc::run_work_queue(&db, &cancel)?;
            // What the app's recovery does next (#120).
            db.remove_stale_tmp_files()?;
            Ok(n)
        });
        out.work_items = match ran {
            Ok(n) => n,
            // A row without any ownership record (killed right after committing it) is foreign
            // as well; the app reports it for the user in the same way.
            Err(e) if e.code == svn_types::ErrorCode::WcBusy => {
                out.locks_need_user += 1;
                return Ok(out);
            }
            Err(e) => return Err(e),
        };
        let s = self.session(url).await?;
        let rec =
            svn_client::commit::recover_wc_commits(&self.journal_dir(), &db, Some(&s), &cancel)
                .await?;
        out.commits_finalized = rec.finalized.len();
        out.commits_aborted = rec.aborted.len();
        out.commits_unsettled = rec
            .needs_user
            .iter()
            .chain(rec.failed.iter())
            .map(|(id, e)| format!("{id}: {e}"))
            .collect();
        Ok(out)
    }
}
