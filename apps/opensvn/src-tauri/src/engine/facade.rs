//! The engine boundary of the app.
//!
//! Every command that touches a repository or a working copy goes through [`EngineFacade`]. The
//! app ships [`FakeEngine`](super::FakeEngine); the real engine implements the same trait. The
//! trait speaks IPC model types so the app layer adds no conversion of its own, and reports
//! failures as [`SvnError`] (mapped to `AppError` once, at the command boundary).
//!
//! Contract notes for implementations:
//! * Paths in arguments and results are `/`-separated paths relative to the working copy root.
//! * Long calls honor `ctx.cancel` at their safe points and report progress through `ctx.progress`.
//!   Cancelling must never leave a half-written file.
//! * Calls on one working copy are serialized by the app (`WcActor`); implementations need no
//!   per-working-copy locking against each other, only against other processes.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use opensvn_ipc::{
    BlameLine, BranchNode, CleanupRequest, CommitRequest, CopyLocationRequest, Depth, FileDiff,
    FileStatus, IgnoreAddRequest, Layout, LogGetRequest, LogPage, RepoEntry, RepoInfo,
    ResolveRequest, Rev, RevertRequest, TextSide, UpdateRequest, WcFormat, WcId,
};
use svn_types::SvnError;

use super::context::OpContext;

/// Result alias of every engine call.
pub type EngineResult<T> = Result<T, SvnError>;

/// A registered working copy as the engine sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WcRef {
    /// Registry id (used for logging and event routing).
    pub id: WcId,
    /// Canonical root directory.
    pub root: PathBuf,
}

/// What the engine knows about a working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WcInfo {
    /// Canonical root directory.
    pub root_path: String,
    /// Repository root URL.
    pub repo_root_url: String,
    /// Repository UUID.
    pub repo_uuid: String,
    /// URL the working copy root points at.
    pub url: String,
    /// Revision of the root.
    pub base_rev: Rev,
    /// Lowest and highest revision when the working copy is mixed.
    pub rev_range: Option<(Rev, Rev)>,
    /// Working copy format (31 or 32).
    pub format: WcFormat,
    /// Whether pristine copies are stored (format 31 always does).
    pub store_pristine: bool,
    /// Path of the root inside the repository, `/`-prefixed.
    pub path_in_repo: String,
}

/// A checkout with its destination already resolved from a path grant.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckoutPlan {
    /// Repository URL.
    pub url: String,
    /// Destination directory (becomes the working copy root).
    pub path: PathBuf,
    /// Revision (`None` = HEAD).
    pub rev: Option<Rev>,
    /// Depth.
    pub depth: Depth,
    /// Sparse entries.
    pub sparse: Option<Vec<opensvn_ipc::SparseEntry>>,
    /// Working copy format.
    pub format: WcFormat,
    /// Store pristine copies.
    pub store_pristine: bool,
    /// Skip externals.
    pub ignore_externals: bool,
}

/// One server-side edit with local paths already resolved from path grants. Fields mirror
/// `opensvn_ipc::RemoteOp`.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Create a directory.
    Mkdir { url: String },
    /// Rename.
    Rename { from_url: String, to_url: String },
    /// Delete.
    Delete { url: String },
    /// Copy.
    Copy {
        from_url: String,
        from_rev: Option<Rev>,
        to_url: String,
    },
    /// Upload a local file the user picked.
    Upload { url: String, local_path: PathBuf },
}

/// A remote edit commit.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEditPlan {
    /// Operations in order.
    pub ops: Vec<EditOp>,
    /// Commit message.
    pub message: String,
}

/// What opening a working copy found and repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Directories whose stale lock (dead owner) or unfinished work was recovered automatically.
    pub recovered: Vec<String>,
    /// Directories locked by a foreign or unknown owner. Never stolen automatically: the UI offers
    /// "Run cleanup" and the user decides.
    pub needs_user_action: Vec<String>,
}

/// Result of [`EngineFacade::open_wc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WcOpened {
    /// Description of the working copy.
    pub info: WcInfo,
    /// Recovery outcome.
    pub recovery: RecoveryReport,
}

/// Which extra nodes `status` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusQuery {
    /// Include ignored nodes.
    pub include_ignored: bool,
    /// Include unversioned nodes.
    pub include_unversioned: bool,
}

/// Diff rendering options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffOptions {
    /// Ignore whitespace-only changes.
    pub ignore_whitespace: bool,
    /// Return full text even above the size limit.
    pub force_large: bool,
}

/// Outcome of a remote status check.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteScan {
    /// Statuses of nodes whose repository side differs, with `remote` filled in.
    pub statuses: Vec<FileStatus>,
    /// Number of revisions in the repository that the working copy has not applied.
    pub incoming: u32,
    /// Newest revision of the repository.
    pub latest_rev: Rev,
}

/// Where a log is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogScope {
    /// A repository URL.
    Url(String),
    /// A path inside a working copy.
    Wc {
        /// Working copy.
        wc: WcRef,
        /// Relative path (`""` for the root).
        path: String,
    },
}

/// Which items a lock or unlock applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockScope {
    /// Paths inside a working copy.
    Wc {
        /// Working copy.
        wc: WcRef,
        /// Relative paths.
        paths: Vec<String>,
    },
    /// Repository URLs.
    Urls(Vec<String>),
}

/// Everything the app asks of an engine.
#[async_trait]
pub trait EngineFacade: Send + Sync {
    // ---- working copy: reads
    /// Describes the working copy containing `root`; `NotAWorkingCopy` if there is none.
    async fn wc_info(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcInfo>;
    /// Makes sure the server session of this working copy is authenticated, prompting the user
    /// through the auth prompter if needed. The app calls it BEFORE taking the working copy write
    /// path, so no prompt ever happens while a working copy lock is held.
    async fn ensure_authenticated(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<()>;
    /// Opens the working copy and runs crash/lock recovery (dead-owner locks and unfinished work
    /// queue items). Called when a working copy is added and at app start; it may write, so the app
    /// calls it through the working copy's write path. Locks of foreign or unknown owners are only
    /// reported in `needs_user_action`.
    async fn open_wc(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcOpened>;
    /// Local status of the working copy (changed nodes, plus locked or changelisted ones).
    async fn status(
        &self,
        wc: &WcRef,
        query: StatusQuery,
        ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>>;
    /// Diff of a working copy path against its base (`against = None`) or a revision.
    async fn diff_wc(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff>;

    // ---- working copy: short writes
    /// Assigns (`Some`) or removes (`None`) a changelist.
    async fn changelist_set(
        &self,
        wc: &WcRef,
        paths: &[String],
        name: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Adds an ignore rule.
    async fn ignore_add(
        &self,
        wc: &WcRef,
        req: &IgnoreAddRequest,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Records that `from` was moved to `to` outside the app.
    async fn mark_move(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Resolves a conflict.
    async fn resolve(&self, wc: &WcRef, req: &ResolveRequest, ctx: &OpContext) -> EngineResult<()>;
    /// Sets (`Some`) or deletes (`None`) a property.
    async fn props_set(
        &self,
        wc: &WcRef,
        path: &str,
        name: &str,
        value: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()>;

    // ---- working copy: long operations
    /// Checks a repository out into `req.path`.
    async fn checkout(&self, plan: &CheckoutPlan, ctx: &OpContext) -> EngineResult<WcInfo>;
    /// Compares the working copy with the repository head.
    async fn status_remote(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<RemoteScan>;
    /// Updates; returns the revision reached.
    async fn update(&self, wc: &WcRef, req: &UpdateRequest, ctx: &OpContext) -> EngineResult<Rev>;
    /// Switches to `url`; returns the revision reached.
    async fn switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Rev>;
    /// Commits; returns the new revision.
    async fn commit(&self, wc: &WcRef, req: &CommitRequest, ctx: &OpContext) -> EngineResult<Rev>;
    /// Reverts local changes.
    async fn revert(&self, wc: &WcRef, req: &RevertRequest, ctx: &OpContext) -> EngineResult<()>;
    /// Schedules paths for addition.
    async fn add_paths(&self, wc: &WcRef, paths: &[String], ctx: &OpContext) -> EngineResult<()>;
    /// Schedules paths for deletion; `keep_local` leaves the files on disk.
    async fn delete_paths(
        &self,
        wc: &WcRef,
        paths: &[String],
        keep_local: bool,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Moves a versioned path.
    async fn move_path(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Cleans up an interrupted working copy.
    async fn cleanup(&self, wc: &WcRef, req: &CleanupRequest, ctx: &OpContext) -> EngineResult<()>;
    /// Changes the recorded depth of a directory.
    async fn set_depth(
        &self,
        wc: &WcRef,
        path: &str,
        depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Upgrades the working copy format.
    async fn upgrade_format(&self, wc: &WcRef, to: u8, ctx: &OpContext) -> EngineResult<()>;

    // ---- repository
    /// Repository facts for a URL.
    async fn repo_info(&self, url: &str, ctx: &OpContext) -> EngineResult<RepoInfo>;
    /// Lists a directory.
    async fn repo_list(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<RepoEntry>>;
    /// Reads a file as text (size-limited).
    async fn repo_cat(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<TextSide>;
    /// Streams a file to `save_to`.
    async fn repo_save(
        &self,
        url: &str,
        rev: Option<Rev>,
        save_to: &Path,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Diff between two revisions of a URL (`from = None` means `to - 1`).
    async fn diff_rev(
        &self,
        url: &str,
        from: Option<Rev>,
        to: Rev,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff>;
    /// Reads one page of the log.
    async fn log(
        &self,
        scope: &LogScope,
        req: &LogGetRequest,
        ctx: &OpContext,
    ) -> EngineResult<LogPage>;
    /// Annotates a file.
    async fn blame(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BlameLine>>;
    /// Locks files.
    async fn lock(
        &self,
        scope: &LockScope,
        comment: Option<&str>,
        steal: bool,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Unlocks files.
    async fn unlock(
        &self,
        scope: &LockScope,
        break_lock: bool,
        ctx: &OpContext,
    ) -> EngineResult<()>;
    /// Applies server-side edits in one commit; returns the new revision.
    async fn remote_edit(&self, plan: &RemoteEditPlan, ctx: &OpContext) -> EngineResult<Rev>;
    /// Copies in the repository (branch, tag); returns the new revision. With `from_wc` the
    /// source is that working copy including its local changes (`req.from_url` and `req.from_rev`
    /// are ignored). The engine reads the working copy with the usual busy-retry.
    async fn copy_location(
        &self,
        req: &CopyLocationRequest,
        from_wc: Option<&WcRef>,
        ctx: &OpContext,
    ) -> EngineResult<Rev>;
    /// Branch tree of a repository for `layout`.
    async fn branches_list(
        &self,
        repo_root_url: &str,
        layout: &Layout,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BranchNode>>;
}
