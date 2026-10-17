//! Caller-driven commit editor.

use std::fs::File;
use std::path::PathBuf;

use async_trait::async_trait;
use svn_types::{
    Checksum, DirId, ErrorCode, FileId, PropChange, RelPath, Result, Revnum, SvnError, Timestamp,
};

use crate::TxnAbort;

/// Message of [`ErrorCode::CommitNotApplied`].
pub const NOTHING_COMMITTED: &str = "Nothing was committed; it is safe to try again.";

/// The error of a commit that failed before its `MERGE` was sent, once its transaction was
/// deleted with the outcome `abort`.
///
/// When the connection broke under one of the commit's writes (`cause` is a connection failure
/// or an unreachable server, see [`SvnError::is_connection_class`]) and the server confirmed it
/// deleted the transaction ([`TxnAbort::Deleted`]), nothing of the commit exists on the server:
/// the result is [`ErrorCode::CommitNotApplied`] with `cause` as its child. Anything else, a
/// transaction whose deletion failed or that was already gone included, keeps `cause` as is:
/// the user then sees what really failed.
///
/// Only for failures before the `MERGE` was sent: once it was, the transaction may have become
/// a revision, whatever a later `DELETE` says.
pub fn unsent_commit_error(cause: SvnError, abort: &Result<TxnAbort>) -> SvnError {
    if cause.is_connection_class() && matches!(abort, Ok(TxnAbort::Deleted)) {
        SvnError::new(ErrorCode::CommitNotApplied, NOTHING_COMMITTED).with_child(cause)
    } else {
        cause
    }
}

/// File content that can be opened again, so uploads survive a 401 or network retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayableSource {
    /// Path of the pristine or working file (already translated when `translated` is true).
    pub path: PathBuf,
    /// Content is the repository-normal form (EOL/keywords translated out).
    pub translated: bool,
    /// Expected size, when known.
    pub size: Option<u64>,
}

impl ReplayableSource {
    /// Opens a fresh read handle; callable any number of times.
    pub fn open(&self) -> std::io::Result<File> {
        File::open(&self.path)
    }
}

/// Result of a successful commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// New revision.
    pub revision: Revnum,
    /// Server commit time.
    pub date: Option<Timestamp>,
    /// Committing user.
    pub author: Option<String>,
    /// Post-commit hook warning, if any.
    pub post_commit_err: Option<String>,
}

/// Observes commit progress so the caller can journal it before content is sent.
pub trait CommitObserver: Send + Sync {
    /// The server transaction exists; record it durably so a crash can clean it up.
    ///
    /// An error vetoes the commit: the RA layer DELETEs the transaction and fails
    /// `commit_editor` with that error, before any content is sent.
    fn on_txn_created(&self, txn: &str) -> Result<()>;

    /// The final `MERGE` of transaction `txn` is about to be sent. From the moment it is sent
    /// the commit may become a revision even if no answer ever arrives, so a journal must
    /// record that the outcome can be unknown before this returns.
    ///
    /// Called once per `close_edit`, after every content request finished and right before
    /// the `MERGE`. An error vetoes the `MERGE`: the transaction is deleted and `close_edit`
    /// fails with that error, so a commit is never merged without the record. The default
    /// does nothing.
    fn on_merge_sending(&self, txn: &str) -> Result<()> {
        let _ = txn;
        Ok(())
    }

    /// The commit was merged into a revision.
    ///
    /// The commit cannot be undone at this point. If this returns an error, `close_edit`
    /// returns it with a message that names the merged revision; callers must treat that as
    /// "merged, bookkeeping failed" and recover the revision from the message or the log.
    fn on_merged(&self, info: &CommitInfo) -> Result<()>;
}

/// Commit editor driven by the caller, like `svn_ra_get_commit_editor`.
/// Paths are relative to the session URL; handle ids come from the editor.
///
/// Cancellation: the editor keeps the `CancelToken` given to `RaSession::commit_editor`. Every
/// method except `abort_edit` returns `Cancelled` once it fires (and `close_edit` then deletes
/// the transaction instead of merging). `abort_edit` always works, also after cancellation, so
/// the caller can clean up.
///
/// `apply_text` may return before the upload has finished (uploads may run in parallel). A
/// failed upload surfaces no later than `close_file` for that file or `close_edit`, and the
/// sources passed to `apply_text` must stay readable until `close_file` for that file returns.
#[async_trait]
pub trait CommitEditor: Send {
    /// Opens the root directory at `base_rev`.
    async fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId>;
    /// Adds a directory, optionally copied from `(URL, revision)` (the source's full URL, as
    /// svn's RA layer takes it).
    async fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId>;
    /// Opens an existing directory.
    async fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId>;
    /// Adds a file, optionally copied from `(URL, revision)`.
    async fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId>;
    /// Opens an existing file.
    async fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId>;
    /// Uploads file text. `base` is the pristine to diff against (None for new files);
    /// both sources must be re-readable for retries.
    async fn apply_text(
        &mut self,
        file: FileId,
        base: Option<ReplayableSource>,
        target: ReplayableSource,
        base_md5: Option<Checksum>,
        result_md5: Checksum,
    ) -> Result<()>;
    /// Changes a directory property.
    async fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()>;
    /// Changes a file property.
    async fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()>;
    /// Deletes an entry of `parent`.
    async fn delete_entry(
        &mut self,
        path: &RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    ) -> Result<()>;
    /// Closes a file. The text sources given to `apply_text` are no longer needed once this
    /// returns, and a local failure (a checksum mismatch, an unreadable source) is reported
    /// here. An implementation may send the file (its text and property changes) in the
    /// background: a failure the server reports then surfaces from a later call, and from
    /// `close_edit` at the latest, before anything is merged.
    async fn close_file(&mut self, file: FileId) -> Result<()>;
    /// Closes a directory.
    async fn close_directory(&mut self, dir: DirId) -> Result<()>;
    /// Finishes the commit (MERGE) and returns the new revision.
    ///
    /// Calls [`CommitObserver::on_merge_sending`] right before the `MERGE`. Fails with
    /// [`svn_types::ErrorCode::CommitOutcomeUnknown`] when the `MERGE` was sent but its result
    /// is unknown; the transaction is then kept for the caller to reconcile (see
    /// [`crate::RaSession::abort_txn_checked`]). Any other error is the server saying it did
    /// not merge. That answer holds for the ordinary refusals (out of date, locks, hooks,
    /// authz), but it is not a proof: mod_dav_svn can also answer an error (for example a 500,
    /// or a 404 for a transaction that is already gone) after the revision was written, when
    /// the repository storage fails during the post-commit steps. A caller that journaled the
    /// `MERGE` therefore reconciles with the repository before it forgets the commit.
    async fn close_edit(self: Box<Self>) -> Result<CommitInfo>;
    /// Abandons the commit and deletes the server transaction: [`crate::TxnAbort::Deleted`]
    /// when the server confirmed it deleted the transaction, [`crate::TxnAbort::Gone`] when it
    /// had none of that name. Any failure to find out is an error, so the transaction may
    /// still exist.
    async fn abort_edit(self: Box<Self>) -> Result<crate::TxnAbort>;
}
