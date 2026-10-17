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
    /// Compare the reported tree with another URL (`svn_ra_do_diff3` without text deltas, as
    /// `svn diff --summarize` needs): the server drives the changes from the reported tree to the
    /// URL at the target revision, like a switch, but sends no file contents. A file whose text
    /// differs still gets an `apply_textdelta`, closed at once with no window, so the editor can
    /// tell a text change from a property change.
    Diff(RepoUrl),
}

impl UpdateMode {
    /// The URL the drive ends at, when it is not the reported one (switch and diff).
    pub fn destination(&self) -> Option<&RepoUrl> {
        match self {
            Self::Switch(url) | Self::Diff(url) => Some(url),
            Self::Update | Self::Status => None,
        }
    }

    /// Whether the drive carries file contents (update and switch).
    pub fn sends_text(&self) -> bool {
        matches!(self, Self::Update | Self::Switch(_))
    }
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

/// Result of [`RaSession::list_counted`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirListing {
    /// Entries the client accepted, as `list` returns them.
    pub entries: Vec<DirEntry>,
    /// Entries the server listed that the client refused (hostile or Windows-reserved names,
    /// another origin). They are not in `entries`; the count lets a caller say that something
    /// is hidden instead of showing a folder as empty. Counts every refused entry the server
    /// sent within the listed depth, whatever its kind.
    pub rejected: usize,
}

/// Parameters of `log`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// Also report the revisions merged in by each entry (`svn log -g`), nested under it
    /// ([`LogEntry::merge_depth`]). Only top-level entries count against `limit`.
    pub include_merged_revisions: bool,
    /// Which revision properties to read.
    pub revprops: LogRevprops,
}

/// Revision properties a log reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LogRevprops {
    /// `svn:author`, `svn:date` and `svn:log`, as `svn log` reads them.
    #[default]
    Default,
    /// Every revision property (`svn log --with-all-revprops`).
    All,
    /// No revision property (`svn log --with-no-revprops`).
    None,
    /// These revision properties (`svn log --with-revprop`); author, date and message are
    /// filled in when listed, the others land in [`LogEntry::revprops`].
    Only(Vec<String>),
}

/// One stretch of a node's history, as reported by
/// [`RaSession::get_location_segments`]: the node lived at `path` in every revision of
/// `range_start..=range_end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationSegment {
    /// Oldest revision of the stretch.
    pub range_start: Revnum,
    /// Youngest revision of the stretch.
    pub range_end: Revnum,
    /// Repository-relative path of the node in these revisions; `None` for a gap, revisions in
    /// which the node's history does not exist (between a copy and the revision it was copied
    /// from).
    pub path: Option<RelPath>,
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

    /// The same session rooted at `url` (same repository, connection settings and
    /// credentials), for a drive anchored elsewhere than this session's URL: an update or
    /// switch of a directory switched away from its parent is anchored at that directory, at
    /// its own URL, like svn's `svn_wc_get_actual_target2`. The default refuses.
    fn reparented(&self, url: &RepoUrl) -> Result<Box<dyn RaSession>> {
        Err(SvnError::new(
            ErrorCode::Internal,
            format!("this session cannot be rooted at {url}"),
        ))
    }

    /// The wcprops (`svn:wc:*`) the RA layer caches with a file it hands to the working copy
    /// outside a report drive, for the file at `repos_relpath` (below the repository root) last
    /// changed in `created_rev`: over DAV, ra_serf's `svn:wc:ra_dav:version-url`, the href the
    /// server would send as the file's `D:checked-in` (`NODES.dav_cache`). None by default.
    fn file_wcprops(&self, repos_relpath: &RelPath, created_rev: Revnum) -> Vec<PropChange> {
        let _ = (repos_relpath, created_rev);
        Vec::new()
    }

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
    /// Like [`list`](Self::list), also counting the entries the client refused. Sessions that
    /// never refuse an entry keep the default, which reports none.
    async fn list_counted(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<DirListing> {
        Ok(DirListing {
            entries: self.list(path, rev, depth, cancel).await?,
            rejected: 0,
        })
    }
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

    /// History of the node at `path`@`peg` (`None` = HEAD) as location segments, youngest
    /// first, like `svn_ra_get_location_segments`: the node at `path` and, through every copy,
    /// the nodes it was copied from. Only the revisions in `end..=start` are reported (`start`
    /// defaults to `peg`, `end` to 0); a segment that reaches beyond them is cropped.
    ///
    /// Unlike the other methods `path` is relative to the **repository root**, not to the
    /// session URL: svn traces another location by reparenting its session, this traces any
    /// location of the repository through the session it has. A node that does not exist at
    /// `peg` fails with `PathNotFound`.
    async fn get_location_segments(
        &self,
        path: &RelPath,
        peg: Option<Revnum>,
        start: Option<Revnum>,
        end: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Vec<LocationSegment>>;

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
