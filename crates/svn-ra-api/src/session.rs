//! Repository session contract.

use std::sync::Arc;

use async_trait::async_trait;
use svn_types::{
    CancelToken, Checksum, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind,
    PropChange, RelPath, RepoDisplayPath, RepoUrl, Reporter, Result, Revnum, SvnError, WindowSink,
};
use tokio::io::AsyncWrite;

use crate::commit::{CommitEditor, CommitObserver};

/// What a report-driven drive does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateMode {
    /// Bring the working copy to the target revision.
    Update,
    /// Re-point the working copy to another URL.
    Switch(RepoUrl),
    /// Compare only; the editor receives the changes without text deltas.
    Status,
}

/// Parameters of a report-driven drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRequest {
    /// Target revision (`None` = HEAD).
    pub revision: Option<Revnum>,
    /// Anchor-relative target (root for a whole-WC update).
    pub target: RelPath,
    /// Requested depth.
    pub depth: Depth,
    /// Update, switch or status.
    pub mode: UpdateMode,
    /// Ask the server to send copy-from information.
    pub send_copyfrom: bool,
    /// Ignore node ancestry.
    pub ignore_ancestry: bool,
}

/// Builds the working-copy report. Runs synchronously on the driver task.
///
/// Report paths are relative to the session URL, like [`UpdateRequest::target`]: the first
/// `set_path` describes the target itself and every other path lies below it. A session
/// translates them to the wire form (svn's target-relative paths, anchored at the target's
/// parent).
pub type ReportFn = Box<dyn FnOnce(&mut dyn Reporter) -> Result<()> + Send>;

/// Result of `get_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    /// Revision the content was read at.
    pub rev: Revnum,
    /// Properties of the file at that revision.
    pub props: Vec<PropChange>,
    /// MD5 of the bytes written to the sink, computed while streaming. Always present, also
    /// when the server reported no checksum to compare it with.
    pub md5: Checksum,
    /// The server reported a checksum for the file and `md5` matched it. `false` means the
    /// content could not be verified against the server (a mismatch is an error, never
    /// `false`); callers that need certainty compare `md5` with a checksum they trust.
    pub verified: bool,
}

/// Parameters of `log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    /// Session-relative paths (empty = root).
    pub paths: Vec<RelPath>,
    /// Newest revision to report (`None` = HEAD).
    pub start: Option<Revnum>,
    /// Oldest revision to report (`None` = 0).
    pub end: Option<Revnum>,
    /// Maximum number of entries (`None` = unlimited).
    pub limit: Option<u32>,
    /// Include changed paths.
    pub discover_changed_paths: bool,
    /// Stop at copies.
    pub strict_node_history: bool,
}

/// One revision of a file as reported by `file_revs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRev {
    /// Absolute repository path at that revision, display only.
    pub path: RepoDisplayPath,
    /// Revision.
    pub rev: Revnum,
    /// Revision properties (author, date, log).
    pub rev_props: Vec<PropChange>,
    /// The revision came from a merge.
    pub merged: bool,
}

/// Receives `file_revs` results.
pub trait FileRevHandler: Send {
    /// Called per revision; return a sink to receive its text delta, or `None` to skip it.
    ///
    /// The delta of a revision is complete only once the sink received the closing
    /// `window(None)`. A revision that did not change the text (only properties) may deliver no
    /// window at all, not even the closing one, before the next `revision` call or the end of
    /// the report; the handler must then keep the previous text unchanged. An empty text is
    /// sent as a closing `window(None)` without data windows.
    fn revision(&mut self, rev: FileRev) -> Result<Option<Box<dyn WindowSink>>>;
}

/// Properties one parent passes down: its repository-relative path (`""` for the root) and its
/// `(name, value)` pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedItem {
    /// Repository-relative path of the parent.
    pub repos_relpath: String,
    /// Properties of the parent.
    pub props: Vec<(String, Vec<u8>)>,
}

/// Repository session. All methods honor `cancel` at the next await point.
#[async_trait]
pub trait RaSession: Send + Sync {
    /// Repository root URL.
    fn repos_root(&self) -> &RepoUrl;
    /// Repository UUID.
    fn uuid(&self) -> &str;
    /// URL this session is rooted at.
    fn session_url(&self) -> &RepoUrl;

    /// User name this session authenticates as, when known (the Basic user name the server
    /// last accepted). `None` before authentication, for anonymous access and for mechanisms
    /// that do not reveal a name (Negotiate). Commits journal it so that recovery can match
    /// the server's `svn:author`; a wrong guess only makes recovery ask the user.
    fn authenticated_user(&self) -> Option<String> {
        None
    }

    /// Authenticates (prompting through `AuthPrompter` if needed). Call BEFORE taking the
    /// working-copy lock so no prompt happens while a lock is held.
    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()>;
    /// Checks that the server answers at all (an `OPTIONS` on the repository root over DAV),
    /// used to tell a network outage from a server that breaks a response. Credentials the
    /// session already has are used; the default asks [`RaSession::ensure_authenticated`].
    async fn probe(&self, cancel: &CancelToken) -> Result<()> {
        self.ensure_authenticated(cancel).await
    }
    /// Drops the idle connections this session would reuse: after a network outage or a
    /// system sleep they are most likely dead. Requests in flight are not affected; settings,
    /// credentials and certificate decisions are kept. The default does nothing.
    fn reset_connections(&self) {}
    /// Newest revision.
    async fn youngest_rev(&self, cancel: &CancelToken) -> Result<Revnum>;
    /// Kind of node at `path`.
    async fn check_path(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<NodeKind>;
    /// Entry for `path`, `None` if absent.
    async fn stat(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Option<DirEntry>>;
    /// Lists a directory.
    async fn list(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<DirEntry>>;
    /// Streams file content into `sink`.
    ///
    /// On error the sink may already hold part of the content, or all of it unverified; the
    /// caller must discard what was written. On success [`FileMeta::md5`] is the MD5 of the
    /// bytes written.
    async fn get_file(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancelToken,
    ) -> Result<FileMeta>;
    /// Streams log entries (newest first) into `on_entry`.
    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
        cancel: &CancelToken,
    ) -> Result<()>;
    /// Locks below `path` up to `depth`.
    async fn get_locks(
        &self,
        path: &RelPath,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<LockInfo>>;
    /// Locks `path`; `steal` takes over an existing lock.
    async fn lock(
        &self,
        path: &RelPath,
        comment: Option<&str>,
        steal: bool,
        base_rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<LockInfo>;
    /// Unlocks `path`; `break_lock` ignores token and owner.
    async fn unlock(
        &self,
        path: &RelPath,
        token: Option<&str>,
        break_lock: bool,
        cancel: &CancelToken,
    ) -> Result<()>;
    /// Reports the revisions of a file in `start..=end` that changed its text.
    ///
    /// Revisions that changed only the file's properties are **not** passed to `handler`
    /// (unlike libsvn_ra_serf, which calls its handler for them with no delta), and property
    /// changes are not reported at all. This is enough for blame; a caller that needs every
    /// revision or the property history (for example `svn blame -g` or a property log) must
    /// not rely on this method as it stands.
    async fn file_revs(
        &self,
        path: &RelPath,
        start: Option<Revnum>,
        end: Option<Revnum>,
        handler: &mut dyn FileRevHandler,
        cancel: &CancelToken,
    ) -> Result<()>;

    /// Properties `repos_relpath` (relative to the repository ROOT, unlike the other paths of
    /// this trait) inherits in `rev`: one item per parent that has properties, the repository
    /// root first, the node itself not included (`svn_ra_get_inherited_props`). The working
    /// copy caches them on its root and on switched nodes. The default reports
    /// `ServerUnsupported`, and callers then leave the cache alone.
    async fn inherited_props(
        &self,
        repos_relpath: &RelPath,
        rev: Revnum,
        cancel: &CancelToken,
    ) -> Result<Vec<InheritedItem>> {
        let _ = (repos_relpath, rev, cancel);
        Err(SvnError::new(
            ErrorCode::ServerUnsupported,
            "this session cannot read inherited properties",
        ))
    }

    /// Update, switch or status: `report` builds the report, then the RA layer drives `editor`
    /// through an [`EditorWorker`](crate::EditorWorker) (editor runs on a blocking thread).
    /// On error or cancel the editor receives `abort_edit`.
    async fn do_update(
        &self,
        req: UpdateRequest,
        report: ReportFn,
        editor: Box<dyn DeltaEditor>,
        cancel: &CancelToken,
    ) -> Result<()>;

    /// Starts a commit. `lock_tokens` are `(path, token)` pairs; the observer is told about the
    /// transaction before any content is sent (an error from the observer deletes the transaction and
    /// fails this call) and about the revision right after MERGE. The returned editor keeps a clone
    /// of `cancel` and honors it (see [`CommitEditor`]).
    async fn commit_editor(
        &self,
        message: &str,
        rev_props: &[PropChange],
        lock_tokens: &[(String, String)],
        keep_locks: bool,
        observer: Arc<dyn CommitObserver>,
        cancel: &CancelToken,
    ) -> Result<Box<dyn CommitEditor>>;

    /// Deletes a leaked server transaction (best effort, idempotent).
    async fn abort_txn(&self, txn_name: &str) -> Result<()>;

    /// Deletes the server transaction `txn_name` and says whether it still existed.
    ///
    /// [`TxnAbort::Deleted`] proves the transaction was not merged (it existed until this call
    /// removed it). [`TxnAbort::Gone`] means the server no longer has it: it was merged into a
    /// revision, or deleted earlier (by a cleanup, an administrator or a failed commit).
    /// [`TxnAbort::Unknown`] means the `DELETE` failed on the way (a server error such as svn
    /// 1.15's "Transaction cleanup failed" when it races the commit's own purge, or a broken
    /// connection): the transaction may still exist, or may have become a revision. Other errors
    /// (authentication, authorization, cancellation, an answer that says neither) leave the
    /// question open and stop the caller.
    ///
    /// The default cannot tell the two cases apart and fails with `ServerUnsupported`, so a
    /// caller never mistakes "unknown" for "deleted".
    async fn abort_txn_checked(&self, txn_name: &str, cancel: &CancelToken) -> Result<TxnAbort> {
        let _ = (txn_name, cancel);
        Err(SvnError::new(
            ErrorCode::ServerUnsupported,
            "this session cannot tell whether a commit transaction still exists",
        ))
    }
}

/// Result of [`RaSession::abort_txn_checked`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnAbort {
    /// The transaction existed and is now deleted: it was never merged.
    Deleted,
    /// The server has no such transaction (already merged or already deleted).
    Gone,
    /// The `DELETE` failed with a server error (5xx) or in transport: whether the transaction
    /// still exists, and whether it was merged, is unknown. Never a proof of either; the reason.
    Unknown(String),
}
