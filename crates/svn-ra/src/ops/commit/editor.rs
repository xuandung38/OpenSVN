//! [`CommitEditor`] over HTTPv2: every editor call becomes a request inside one transaction.
//!
//! Directories are created (`MKCOL`/`COPY`) and entries deleted at once. File text is uploaded
//! by background tasks (bounded), and property changes are collected and sent as one
//! `PROPPATCH` when the node is closed, after the file's upload has finished.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use svn_protocol::TxnName;
use svn_protocol::commit::{
    LockToken, copy_request, delete_request, mkcol_request, proppatch_request,
};
use svn_ra_api::{CommitEditor, CommitInfo, CommitObserver, ReplayableSource};
use svn_types::{
    CancelToken, Checksum, DirId, ErrorCode, FileId, NodeKind, PropChange, RelPath, Result, Revnum,
    SvnError,
};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::merge::{check_proppatch, delete_txn, merge};
use super::put::{PutJob, put_empty, require_md5, run_put};
use crate::ops::update::RaWire;

/// A background upload that is aborted when dropped.
struct Upload(JoinHandle<Result<()>>);

impl Upload {
    async fn finish(mut self) -> Result<()> {
        match (&mut self.0).await {
            Ok(r) => r,
            Err(e) => Err(SvnError::new(
                ErrorCode::Internal,
                format!("upload task failed: {e}"),
            )),
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct DirState {
    path: RelPath,
    base_rev: Option<Revnum>,
    props: Vec<PropChange>,
}

struct FileState {
    path: RelPath,
    base_rev: Option<Revnum>,
    added: bool,
    copied: bool,
    /// `apply_text` was called.
    text_sent: bool,
    props: Vec<PropChange>,
    upload: Option<Upload>,
}

/// Commit editor bound to one server transaction.
///
/// End every edit with [`CommitEditor::close_edit`] or [`CommitEditor::abort_edit`]. An editor
/// dropped without either (a panic, or the caller dropping the future) cannot wait for the
/// server, so the transaction may outlive it: when a Tokio runtime is available the drop spawns
/// a best-effort `DELETE` of the transaction, unless a `MERGE` was already sent (then the
/// outcome is unknown and deleting could race the commit). Callers that need certainty journal
/// the name passed to [`CommitObserver::on_txn_created`] and remove leftovers with
/// [`super::abort_txn`].
pub struct HttpCommitEditor {
    wire: RaWire,
    txn: TxnName,
    cancel: CancelToken,
    observer: Arc<dyn CommitObserver>,
    /// `(session-relative path, token)` of the locks the commit uses.
    locks: Vec<(RelPath, String)>,
    keep_locks: bool,
    next_id: u32,
    dirs: HashMap<u32, DirState>,
    files: HashMap<u32, FileState>,
    /// Session-relative paths deleted so far; mod_dav removes the lock of a deleted node itself.
    deleted: HashSet<RelPath>,
    slots: Arc<Semaphore>,
    /// The edit ended (`close_edit` or `abort_edit` ran), or a `MERGE` was sent: `Drop` must not
    /// delete the transaction.
    settled: bool,
}

impl HttpCommitEditor {
    pub(super) fn new(
        wire: RaWire,
        txn: TxnName,
        cancel: CancelToken,
        observer: Arc<dyn CommitObserver>,
        locks: Vec<(RelPath, String)>,
        keep_locks: bool,
    ) -> Self {
        let slots = Arc::new(Semaphore::new(wire.connections().clamp(1, 4)));
        Self {
            wire,
            txn,
            cancel,
            observer,
            locks,
            keep_locks,
            next_id: 0,
            dirs: HashMap::new(),
            files: HashMap::new(),
            deleted: HashSet::new(),
            slots,
            settled: false,
        }
    }

    /// Name of the server transaction (journaled by the observer before any content is sent).
    pub fn txn_name(&self) -> &str {
        &self.txn.name
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn lock_for(&self, path: &RelPath) -> Option<&str> {
        self.locks
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, t)| t.as_str())
    }

    /// `If:` header value naming every lock at or below `path` (session-relative) that has not
    /// been deleted yet, like libsvn_ra_serf's `setup_if_header_recursive`. Deleting a directory
    /// needs the tokens of the locked files inside it; so does creating (`MKCOL`) or copying a
    /// directory over a path that was just deleted, because mod_dav still sees the locks of the
    /// old nodes below it (a DAV lock may name a path that does not exist).
    fn recursive_if_header(&self, path: &RelPath) -> Result<Option<String>> {
        let mut parts = Vec::new();
        for (p, token) in &self.locks {
            if !p.starts_with(path) || self.deleted.contains(p) {
                continue;
            }
            let url = self.wire.urls().public(&self.wire.repo_path(p)?)?;
            parts.push(format!("<{url}> (<{}>)", checked_token(token)?));
        }
        Ok((!parts.is_empty()).then(|| parts.join(" ")))
    }

    fn dir(&self, id: DirId, what: &str) -> Result<&DirState> {
        self.dirs
            .get(&id.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, format!("{what}: unknown directory")))
    }

    /// Resolves a copy source to a repository-relative path. Accepted forms:
    ///
    /// - an absolute `http(s)://` URL below the repository root of this server (what
    ///   libsvn_ra_serf accepts), percent-encoded like any URL;
    /// - an absolute repository path such as `/trunk/a` (not percent-encoded), which is
    ///   relative to the repository root, not to the server or the session URL.
    fn copy_source(&self, from: &str) -> Result<RelPath> {
        if from.contains("://") {
            self.wire.repo_path_of_url(from)
        } else if let Some(path) = from.strip_prefix('/') {
            RelPath::new(path.trim_end_matches('/')).map_err(|e| {
                SvnError::new(
                    ErrorCode::UnsafePath,
                    format!(
                        "copy source {from:?} is not a valid repository path: {}",
                        e.message
                    ),
                )
            })
        } else {
            Err(SvnError::new(
                ErrorCode::UnsafePath,
                format!("copy source {from:?} is neither a URL nor an absolute repository path"),
            ))
        }
    }

    /// `COPY` into the transaction. A directory copy carries the recursive `If:` header for the
    /// same reason as `MKCOL` (see [`Self::recursive_if_header`]).
    async fn copy_node(&self, dst: &RelPath, from: (&str, Revnum), kind: NodeKind) -> Result<()> {
        let src = self.copy_source(from.0)?;
        let repo_dst = self.wire.repo_path(dst)?;
        let mut req = copy_request(self.wire.urls(), from.1, &src, kind, &self.txn, &repo_dst)?;
        if kind == NodeKind::Dir
            && let Some(header) = self.recursive_if_header(dst)?
        {
            req = req.header("If", header);
        }
        self.wire.send_ok(req, &self.cancel).await?;
        Ok(())
    }

    async fn proppatch(
        &self,
        session_path: &RelPath,
        repo_path: &RelPath,
        base_rev: Option<Revnum>,
        props: &[PropChange],
    ) -> Result<()> {
        if props.is_empty() {
            return Ok(());
        }
        let lock = self.lock_for(session_path).map(LockToken);
        let req = proppatch_request(
            self.wire.urls(),
            &self.txn,
            repo_path,
            props,
            base_rev,
            lock,
        )?;
        let resp = self.wire.send_ok(req, &self.cancel).await?;
        check_proppatch(&RaWire::read_small(resp, &self.cancel).await?)
    }

    /// Deletes the transaction after a failure; its own failure must not hide the cause.
    async fn discard_txn(&self) {
        // Cleanup must work after cancellation, so it uses a fresh token.
        let _ = delete_txn(&self.wire, &self.txn, &CancelToken::new()).await;
    }

    async fn finish_upload(&mut self, file: u32) -> Result<()> {
        if let Some(up) = self.files.get_mut(&file).and_then(|f| f.upload.take()) {
            up.finish().await?;
        }
        Ok(())
    }

    async fn close_edit_inner(&mut self) -> Result<CommitInfo> {
        self.cancel.check()?;
        if !self.files.is_empty() || !self.dirs.is_empty() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "commit editor closed while files or directories are still open",
            ));
        }
        let tokens = self.locks.clone();
        // From here on the commit may succeed on the server even if this future is dropped.
        self.settled = true;
        let merged = merge(
            &self.wire,
            &self.txn,
            self.keep_locks,
            &tokens,
            &self.cancel,
        )
        .await?;
        Ok(CommitInfo {
            revision: merged.new_rev,
            date: merged.date,
            author: merged.author,
            post_commit_err: merged.post_commit_error,
        })
    }
}

#[async_trait]
impl CommitEditor for HttpCommitEditor {
    async fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId> {
        self.cancel.check()?;
        if !self.dirs.is_empty() || self.next_id != 0 {
            return Err(SvnError::new(ErrorCode::Internal, "root already opened"));
        }
        let id = self.fresh_id();
        self.dirs.insert(
            id,
            DirState {
                path: RelPath::root(),
                base_rev,
                props: Vec::new(),
            },
        );
        Ok(DirId(id))
    }

    async fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        self.cancel.check()?;
        self.dir(parent, "add_directory")?;
        let repo = self.wire.repo_path(path)?;
        match copy_from {
            Some(from) => self.copy_node(path, from, NodeKind::Dir).await?,
            None => {
                let mut req = mkcol_request(self.wire.urls(), &self.txn, &repo)?;
                // Like ra_serf's setup_add_dir_common_headers: no keep-locks option here, the
                // locks are kept or released by the MERGE.
                if let Some(header) = self.recursive_if_header(path)? {
                    req = req.header("If", header);
                }
                self.wire.send_ok(req, &self.cancel).await?;
            }
        }
        let id = self.fresh_id();
        self.dirs.insert(
            id,
            DirState {
                path: path.clone(),
                base_rev: None,
                props: Vec::new(),
            },
        );
        Ok(DirId(id))
    }

    async fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        self.cancel.check()?;
        self.dir(parent, "open_directory")?;
        let id = self.fresh_id();
        self.dirs.insert(
            id,
            DirState {
                path: path.clone(),
                base_rev,
                props: Vec::new(),
            },
        );
        Ok(DirId(id))
    }

    async fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        self.cancel.check()?;
        self.dir(parent, "add_file")?;
        if let Some(from) = copy_from {
            self.copy_node(path, from, NodeKind::File).await?;
        }
        let id = self.fresh_id();
        self.files.insert(
            id,
            FileState {
                path: path.clone(),
                base_rev: None,
                added: true,
                copied: copy_from.is_some(),
                text_sent: false,
                props: Vec::new(),
                upload: None,
            },
        );
        Ok(FileId(id))
    }

    async fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        self.cancel.check()?;
        self.dir(parent, "open_file")?;
        let id = self.fresh_id();
        self.files.insert(
            id,
            FileState {
                path: path.clone(),
                base_rev,
                added: false,
                copied: false,
                text_sent: false,
                props: Vec::new(),
                upload: None,
            },
        );
        Ok(FileId(id))
    }

    async fn apply_text(
        &mut self,
        file: FileId,
        base: Option<ReplayableSource>,
        target: ReplayableSource,
        base_md5: Option<Checksum>,
        result_md5: Checksum,
    ) -> Result<()> {
        self.cancel.check()?;
        // The text is sent as a full-text svndiff; the base only matters through its checksum.
        let _ = base;
        let result_md5 = require_md5(&result_md5, "result checksum")?;
        let base_md5 = base_md5
            .as_ref()
            .map(|c| require_md5(c, "base checksum"))
            .transpose()?;
        let (path, base_rev) = {
            let f = self
                .files
                .get(&file.0)
                .ok_or_else(|| SvnError::new(ErrorCode::Internal, "apply_text: unknown file"))?;
            if f.upload.is_some() {
                return Err(SvnError::new(
                    ErrorCode::Internal,
                    format!("text of {} applied twice", f.path),
                ));
            }
            (f.path.clone(), f.base_rev)
        };
        let job = PutJob {
            wire: self.wire.clone(),
            txn: self.txn.clone(),
            path: self.wire.repo_path(&path)?,
            target,
            base_rev,
            base_md5,
            result_md5,
            lock: self.lock_for(&path).map(str::to_owned),
            slots: Arc::clone(&self.slots),
            cancel: self.cancel.clone(),
        };
        let upload = Upload(tokio::spawn(run_put(job)));
        if let Some(f) = self.files.get_mut(&file.0) {
            f.upload = Some(upload);
            f.text_sent = true;
        }
        Ok(())
    }

    async fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        self.cancel.check()?;
        self.dirs
            .get_mut(&dir.0)
            .ok_or_else(|| {
                SvnError::new(ErrorCode::Internal, "change_dir_prop: unknown directory")
            })?
            .props
            .push(change);
        Ok(())
    }

    async fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        self.cancel.check()?;
        self.files
            .get_mut(&file.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "change_file_prop: unknown file"))?
            .props
            .push(change);
        Ok(())
    }

    async fn delete_entry(
        &mut self,
        path: &RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    ) -> Result<()> {
        self.cancel.check()?;
        self.dir(parent, "delete_entry")?;
        let repo = self.wire.repo_path(path)?;
        let mut req = delete_request(self.wire.urls(), &self.txn, &repo, rev, None)?;
        if let Some(header) = self.recursive_if_header(path)? {
            req = req.header("If", header);
            if self.keep_locks {
                req = req.header("X-SVN-Options", "keep-locks");
            }
        }
        self.wire.send_ok(req, &self.cancel).await?;
        self.deleted.insert(path.clone());
        Ok(())
    }

    async fn close_file(&mut self, file: FileId) -> Result<()> {
        self.cancel.check()?;
        self.finish_upload(file.0).await?;
        let state = self
            .files
            .remove(&file.0)
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "close_file: unknown file"))?;
        let repo = self.wire.repo_path(&state.path)?;
        // A new file without text still has to exist.
        if state.added && !state.copied && !state.text_sent {
            let lock = self.lock_for(&state.path).map(str::to_owned);
            put_empty(&self.wire, &self.txn, &repo, lock.as_deref(), &self.cancel).await?;
        }
        self.proppatch(&state.path, &repo, state.base_rev, &state.props)
            .await
    }

    async fn close_directory(&mut self, dir: DirId) -> Result<()> {
        self.cancel.check()?;
        let state = self.dirs.remove(&dir.0).ok_or_else(|| {
            SvnError::new(ErrorCode::Internal, "close_directory: unknown directory")
        })?;
        let repo = self.wire.repo_path(&state.path)?;
        self.proppatch(&state.path, &repo, state.base_rev, &state.props)
            .await
    }

    async fn close_edit(mut self: Box<Self>) -> Result<CommitInfo> {
        let info = match self.close_edit_inner().await {
            Ok(info) => info,
            Err(e) => {
                self.discard_txn().await;
                self.settled = true;
                return Err(e);
            }
        };
        if let Err(e) = self.observer.on_merged(&info) {
            return Err(SvnError::new(
                e.code,
                format!(
                    "the commit was merged as revision {} but recording it failed: {}",
                    info.revision, e.message
                ),
            ));
        }
        Ok(info)
    }

    async fn abort_edit(mut self: Box<Self>) -> Result<()> {
        // Stop uploads first so nothing writes into the transaction while it is deleted.
        drop(std::mem::take(&mut self.files));
        self.settled = true;
        delete_txn(&self.wire, &self.txn, &CancelToken::new()).await
    }
}

impl Drop for HttpCommitEditor {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // Uploads stop with their handles; then the transaction is deleted in the background.
        drop(std::mem::take(&mut self.files));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let wire = self.wire.clone();
            let txn = self.txn.clone();
            rt.spawn(async move {
                let _ = delete_txn(&wire, &txn, &CancelToken::new()).await;
            });
        }
    }
}

/// A lock token as it may appear in an `If:` header (`opaquelocktoken:...`, no brackets,
/// whitespace or controls).
fn checked_token(token: &str) -> Result<&str> {
    let t = token.trim().trim_start_matches('<').trim_end_matches('>');
    let ok = t.len() <= 256
        && t.strip_prefix("opaquelocktoken:")
            .is_some_and(|r| !r.is_empty())
        && !t
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')'));
    if ok {
        Ok(t)
    } else {
        Err(SvnError::new(
            ErrorCode::LockTokenInvalid,
            "malformed lock token",
        ))
    }
}
