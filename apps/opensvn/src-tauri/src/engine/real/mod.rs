//! The real engine: [`EngineFacade`] backed by the Subversion crates.
//!
//! * Repository reads, locks, branch listing and server-side edits go through [`RaSession`]
//!   (`remote`, `blame`, `layout`, `remote_edit`, `locks`).
//! * Opening a working copy reads `wc.db` through `svn-wc` and runs crash recovery (`wc_read`,
//!   `recovery`, `journal`).
//! * Status, working copy diff and remote status run through `svn-client` (`wc_status`, with the
//!   type mapping in `wc_map`).
//! * Operations that need the rest of the `svn-client` working copy layer (checkout, update,
//!   switch, commit, revert, add/delete/move, cleanup, depth, format upgrade, resolve, ignore,
//!   properties, changelists) are not wired yet and fail with
//!   [`ErrorCode::Internal`] carrying a `not wired` message. Each one delegates to a `do_*` method
//!   in its own file (`wc_status`, `sync`, `commit`, `local`), so wiring one area later touches
//!   only that file.
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
mod commit;
mod journal;
mod layout;
mod local;
mod locks;
mod recovery;
mod remote;
mod remote_edit;
mod sync;
mod text;
mod wc_map;
mod wc_status;

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

    /// Connects to the repository containing `url` with explicit options.
    ///
    /// The default ignores `options` and calls [`SessionConnector::connect`], so the session is
    /// rooted at the repository root; connectors that can anchor a session at `url` or open it
    /// without prompting override this.
    ///
    /// Real connectors MUST override it: the default cannot honor `options.interactive`, so a
    /// background call (`interactive == false`) on a connector that keeps the default may still
    /// prompt for credentials. Only in-memory test connectors may rely on the default.
    async fn connect_with(
        &self,
        url: &RepoUrl,
        options: &ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        let _ = options;
        self.connect(url, cancel).await
    }
}

/// Where a session opened by [`SessionConnector::connect_with`] is anchored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// At the repository root (`session_url() == repos_root()`), as [`SessionConnector::connect`].
    #[default]
    Root,
    /// At the requested URL (checkout, update, switch and remote status run against it).
    Url,
}

/// Options of [`SessionConnector::connect_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Where the session is anchored.
    pub anchor: Anchor,
    /// False for background work: the session must fail with `auth-required` instead of prompting.
    pub interactive: bool,
}

impl ConnectOptions {
    /// Root-anchored session with the interactivity of `ctx`.
    pub fn root(ctx: &OpContext) -> Self {
        Self {
            anchor: Anchor::Root,
            interactive: ctx.interactive,
        }
    }

    /// Session anchored at the requested URL with the interactivity of `ctx`.
    pub fn at_url(ctx: &OpContext) -> Self {
        Self {
            anchor: Anchor::Url,
            interactive: ctx.interactive,
        }
    }
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

    /// Context for `svn-client` operations: working copy locks are recorded as owned by this
    /// process, with default client configuration and no progress sink.
    pub(crate) fn client_ctx(&self, ctx: &OpContext) -> svn_client::ClientCtx {
        let _ = ctx; // progress and configuration are mapped by the packages that wire operations
        svn_client::ClientCtx::new(self.lock_owner())
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

    async fn ensure_authenticated(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<()> {
        if !ctx.interactive {
            return Ok(()); // background work never prompts
        }
        let info = wc_read::wc_info(&wc.root, ctx).await?;
        self.locate_repo_path(&info.repo_root_url, &RelPath::root(), ctx)
            .await
            .map(|_| ())
    }

    async fn open_wc(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcOpened> {
        self.open_wc_with_recovery(root, ctx).await
    }

    async fn status(
        &self,
        wc: &WcRef,
        query: StatusQuery,
        ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>> {
        self.do_status(wc, query, ctx).await
    }

    async fn diff_wc(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        self.do_diff_wc(wc, path, against, opts, ctx).await
    }

    async fn changelist_set(
        &self,
        wc: &WcRef,
        paths: &[String],
        name: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_changelist_set(wc, paths, name, ctx).await
    }

    async fn ignore_add(
        &self,
        wc: &WcRef,
        req: &IgnoreAddRequest,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_ignore_add(wc, req, ctx).await
    }

    async fn mark_move(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_mark_move(wc, from, to, ctx).await
    }

    async fn resolve(&self, wc: &WcRef, req: &ResolveRequest, ctx: &OpContext) -> EngineResult<()> {
        self.do_resolve(wc, req, ctx).await
    }

    async fn props_set(
        &self,
        wc: &WcRef,
        path: &str,
        name: &str,
        value: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_props_set(wc, path, name, value, ctx).await
    }

    async fn checkout(&self, plan: &CheckoutPlan, ctx: &OpContext) -> EngineResult<WcInfo> {
        self.do_checkout(plan, ctx).await
    }

    async fn status_remote(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<RemoteScan> {
        self.do_status_remote(wc, ctx).await
    }

    async fn update(&self, wc: &WcRef, req: &UpdateRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_update(wc, req, ctx).await
    }

    async fn switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        self.do_switch(wc, url, rev, ctx).await
    }

    async fn commit(&self, wc: &WcRef, req: &CommitRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_commit(wc, req, ctx).await
    }

    async fn revert(&self, wc: &WcRef, req: &RevertRequest, ctx: &OpContext) -> EngineResult<()> {
        self.do_revert(wc, req, ctx).await
    }

    async fn add_paths(&self, wc: &WcRef, paths: &[String], ctx: &OpContext) -> EngineResult<()> {
        self.do_add_paths(wc, paths, ctx).await
    }

    async fn delete_paths(
        &self,
        wc: &WcRef,
        paths: &[String],
        keep_local: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_delete_paths(wc, paths, keep_local, ctx).await
    }

    async fn move_path(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_move_path(wc, from, to, ctx).await
    }

    async fn cleanup(&self, wc: &WcRef, req: &CleanupRequest, ctx: &OpContext) -> EngineResult<()> {
        self.do_cleanup(wc, req, ctx).await
    }

    async fn set_depth(
        &self,
        wc: &WcRef,
        path: &str,
        depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_set_depth(wc, path, depth, ctx).await
    }

    async fn upgrade_format(&self, wc: &WcRef, to: u8, ctx: &OpContext) -> EngineResult<()> {
        self.do_upgrade_format(wc, to, ctx).await
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

    async fn copy_location(
        &self,
        req: &CopyLocationRequest,
        from_wc: Option<&WcRef>,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        match from_wc {
            Some(wc) => self.do_copy_location_from_wc(req, wc, ctx).await,
            None => self.do_copy_location(req, ctx).await,
        }
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
