//! The real engine: [`EngineFacade`] backed by the Subversion crates.
//!
//! * Repository reads, locks, branch listing and server-side edits go through [`RaSession`]
//!   (`remote`, `blame`, `layout`, `remote_edit`, `locks`).
//! * Opening a working copy reads `wc.db` through `svn-wc` and runs crash recovery (`wc_read`,
//!   `recovery`, `journal`).
//! * Operations that need the `svn-client` working copy layer (checkout, status, diff, update,
//!   switch, commit, revert, add/delete/move, cleanup, depth, format upgrade, resolve, ignore,
//!   properties, changelists, remote status) are not wired yet and fail with
//!   [`ErrorCode::Internal`] carrying a `not wired` message; each one is a single method in this
//!   file, so wiring it later does not touch the rest.
//!
//! Repositories are reached through a [`SessionConnector`], so the same engine runs against the
//! HTTP transport in the app and against the in-memory repository in tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use opensvn_ipc::{
    BlameLine, BranchNode, CleanupRequest, CommitRequest, CopyLocationRequest, Depth, FileDiff,
    FileStatus, IgnoreAddRequest, Layout, LogGetRequest, LogPage, RepoEntry, RepoInfo,
    ResolveRequest, Rev, RevertRequest, TextSide, UpdateRequest,
};
use svn_ra_api::RaSession;
use svn_types::{CancelToken, ErrorCode, LockOwnershipStore, RelPath, RepoUrl, SvnError};

use super::context::OpContext;
use super::facade::{
    CheckoutPlan, DiffOptions, EngineFacade, EngineResult, LockScope, LogScope, RemoteEditPlan,
    RemoteScan, StatusQuery, WcInfo, WcOpened, WcRef,
};

mod blame;
mod journal;
mod layout;
mod locks;
mod recovery;
mod remote;
mod remote_edit;
mod text;

use crate::engine::text::percent_decode;
mod wc_read;

#[cfg(test)]
mod tests;

pub use journal::{CommitJournal, JournalPhase, JournalRecord, JournalStore};
pub use recovery::{RecoveryHooks, UnwiredRecovery};

/// Opens repository sessions for the engine.
///
/// The returned session must be rooted at the repository root (`session_url() == repos_root()`),
/// whatever `url` points at: the engine addresses everything as a path below the root. The
/// implementation decides about pooling and reuse; the engine calls `ensure_authenticated` on the
/// session before reading or writing.
#[async_trait]
pub trait SessionConnector: Send + Sync {
    /// Connects to the repository containing `url`.
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError>;
}

/// Everything the real engine needs from the app.
pub struct RealEngineConfig {
    /// Opens repository sessions.
    pub connector: Arc<dyn SessionConnector>,
    /// Records which process holds which working copy directory lock.
    pub lock_ownership: Arc<dyn LockOwnershipStore>,
    /// Boot session id of this process (`lock_ownership::current_boot_id()` in the app); with
    /// the process id it identifies the locks this process takes.
    pub boot_id: String,
    /// Per-user state directory; commit journals live below it.
    pub state_dir: PathBuf,
    /// Steps of crash recovery that need the working copy layer.
    pub recovery: Arc<dyn RecoveryHooks>,
}

/// [`EngineFacade`] implementation on top of the Subversion crates.
pub struct RealEngine {
    connector: Arc<dyn SessionConnector>,
    lock_ownership: Arc<dyn LockOwnershipStore>,
    boot_id: String,
    journals: JournalStore,
    recovery: Arc<dyn RecoveryHooks>,
}

impl RealEngine {
    /// Creates the engine.
    pub fn new(config: RealEngineConfig) -> Self {
        Self {
            connector: config.connector,
            lock_ownership: config.lock_ownership,
            boot_id: config.boot_id,
            journals: JournalStore::new(config.state_dir),
            recovery: config.recovery,
        }
    }

    /// Opens a session for `url` and returns it with the path of `url` below the repository root.
    pub(crate) async fn locate(
        &self,
        url: &str,
        ctx: &OpContext,
    ) -> EngineResult<(Arc<dyn RaSession>, RelPath)> {
        ctx.check()?;
        let parsed = RepoUrl::parse(url)?;
        let session = self.connector.connect(&parsed, &ctx.cancel).await?;
        let path = path_below_root(session.as_ref(), &parsed)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        Ok((session, path))
    }

    /// Opens a session for the repository at `root`; used when the location comes from a working
    /// copy record instead of a URL, so `path` is already relative to the root.
    pub(crate) async fn locate_repo_path(
        &self,
        root: &str,
        path: &RelPath,
        ctx: &OpContext,
    ) -> EngineResult<(Arc<dyn RaSession>, RelPath)> {
        ctx.check()?;
        let parsed = RepoUrl::parse(root)?;
        let session = self.connector.connect(&parsed, &ctx.cancel).await?;
        path_below_root(session.as_ref(), &parsed)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        Ok((session, path.clone()))
    }
}

/// Path of `url` below the session's repository root.
fn path_below_root(session: &dyn RaSession, url: &RepoUrl) -> EngineResult<RelPath> {
    if session.session_url() != session.repos_root() {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "session is not rooted at the repository root",
        ));
    }
    let root = session.repos_root().to_string();
    let full = url.to_string();
    let rest = full
        .strip_prefix(&root)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .ok_or_else(|| {
            SvnError::new(
                ErrorCode::Internal,
                format!("{full} is not inside repository {root}"),
            )
        })?;
    RelPath::new(&percent_decode(rest.trim_matches('/')))
}

/// Error of an operation that waits for the working copy layer.
fn not_wired(op: &str) -> SvnError {
    SvnError::new(
        ErrorCode::Internal,
        format!("not wired: {op} needs the svn-client working copy operations"),
    )
}

#[async_trait]
impl EngineFacade for RealEngine {
    async fn wc_info(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcInfo> {
        wc_read::wc_info(root, ctx).await
    }

    async fn open_wc(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcOpened> {
        self.open_wc_with_recovery(root, ctx).await
    }

    async fn status(
        &self,
        _wc: &WcRef,
        _query: StatusQuery,
        _ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>> {
        Err(not_wired("status"))
    }

    async fn diff_wc(
        &self,
        _wc: &WcRef,
        _path: &str,
        _against: Option<Rev>,
        _opts: DiffOptions,
        _ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        Err(not_wired("diff_wc"))
    }

    async fn changelist_set(
        &self,
        _wc: &WcRef,
        _paths: &[String],
        _name: Option<&str>,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("changelist_set"))
    }

    async fn ignore_add(
        &self,
        _wc: &WcRef,
        _req: &IgnoreAddRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("ignore_add"))
    }

    async fn mark_move(
        &self,
        _wc: &WcRef,
        _from: &str,
        _to: &str,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("mark_move"))
    }

    async fn resolve(
        &self,
        _wc: &WcRef,
        _req: &ResolveRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("resolve"))
    }

    async fn props_set(
        &self,
        _wc: &WcRef,
        _path: &str,
        _name: &str,
        _value: Option<&str>,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("props_set"))
    }

    async fn checkout(&self, _plan: &CheckoutPlan, ctx: &OpContext) -> EngineResult<WcInfo> {
        // Cancellation wins over everything else, as for every wired operation.
        ctx.check()?;
        Err(not_wired("checkout"))
    }

    async fn status_remote(&self, _wc: &WcRef, _ctx: &OpContext) -> EngineResult<RemoteScan> {
        Err(not_wired("status_remote"))
    }

    async fn update(
        &self,
        _wc: &WcRef,
        _req: &UpdateRequest,
        _ctx: &OpContext,
    ) -> EngineResult<Rev> {
        Err(not_wired("update"))
    }

    async fn switch(
        &self,
        _wc: &WcRef,
        _url: &str,
        _rev: Option<Rev>,
        _ctx: &OpContext,
    ) -> EngineResult<Rev> {
        Err(not_wired("switch"))
    }

    async fn commit(
        &self,
        _wc: &WcRef,
        _req: &CommitRequest,
        _ctx: &OpContext,
    ) -> EngineResult<Rev> {
        Err(not_wired("commit"))
    }

    async fn revert(
        &self,
        _wc: &WcRef,
        _req: &RevertRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("revert"))
    }

    async fn add_paths(
        &self,
        _wc: &WcRef,
        _paths: &[String],
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("add_paths"))
    }

    async fn delete_paths(
        &self,
        _wc: &WcRef,
        _paths: &[String],
        _keep_local: bool,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("delete_paths"))
    }

    async fn move_path(
        &self,
        _wc: &WcRef,
        _from: &str,
        _to: &str,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("move_path"))
    }

    async fn cleanup(
        &self,
        _wc: &WcRef,
        _req: &CleanupRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("cleanup"))
    }

    async fn set_depth(
        &self,
        _wc: &WcRef,
        _path: &str,
        _depth: Depth,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        Err(not_wired("set_depth"))
    }

    async fn upgrade_format(&self, _wc: &WcRef, _to: u8, _ctx: &OpContext) -> EngineResult<()> {
        Err(not_wired("upgrade_format"))
    }

    async fn repo_info(&self, url: &str, ctx: &OpContext) -> EngineResult<RepoInfo> {
        self.do_repo_info(url, ctx).await
    }

    async fn repo_list(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<RepoEntry>> {
        self.do_repo_list(url, rev, ctx).await
    }

    async fn repo_cat(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<TextSide> {
        self.do_repo_cat(url, rev, ctx).await
    }

    async fn repo_save(
        &self,
        url: &str,
        rev: Option<Rev>,
        save_to: &Path,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_repo_save(url, rev, save_to, ctx).await
    }

    async fn diff_rev(
        &self,
        url: &str,
        from: Option<Rev>,
        to: Rev,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        self.do_diff_rev(url, from, to, opts, ctx).await
    }

    async fn log(
        &self,
        scope: &LogScope,
        req: &LogGetRequest,
        ctx: &OpContext,
    ) -> EngineResult<LogPage> {
        self.do_log(scope, req, ctx).await
    }

    async fn blame(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BlameLine>> {
        self.do_blame(url, rev, ctx).await
    }

    async fn lock(
        &self,
        scope: &LockScope,
        comment: Option<&str>,
        steal: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_lock(scope, comment, steal, ctx).await
    }

    async fn unlock(
        &self,
        scope: &LockScope,
        break_lock: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_unlock(scope, break_lock, ctx).await
    }

    async fn remote_edit(&self, plan: &RemoteEditPlan, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_remote_edit(plan, ctx).await
    }

    async fn copy_location(&self, req: &CopyLocationRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_copy_location(req, ctx).await
    }

    async fn branches_list(
        &self,
        repo_root_url: &str,
        layout: &Layout,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BranchNode>> {
        self.do_branches_list(repo_root_url, layout, ctx).await
    }
}
