//! RA session decorator for commit tests: records the editor drive, can replace the server with
//! a record-only editor, and injects cancellation or failures.
#![allow(dead_code, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{
    CommitEditor, CommitInfo, CommitObserver, FileMeta, FileRevHandler, LogRequest, RaSession,
    ReplayableSource, ReportFn, TxnAbort, UpdateRequest,
};
use svn_types::{
    CancelToken, Checksum, DeltaEditor, Depth, DirEntry, DirId, ErrorCode, FileId, LockInfo,
    LogEntry, NodeKind, PropChange, RelPath, RepoUrl, Result, Revnum, SvnError,
};
use tokio::io::AsyncWrite;

/// Shared event log.
pub type Log = Arc<Mutex<Vec<String>>>;

/// Hook invoked when `ensure_authenticated` runs.
pub type AuthHook = Box<dyn Fn() + Send + Sync>;

pub struct RecRa {
    pub inner: FakeRaSession,
    pub log: Log,
    /// Do not touch the fake repository: accept everything and answer `next_rev`.
    pub record_only: bool,
    pub next_rev: u64,
    /// Cancel this token when the first text upload starts.
    pub cancel_on_text: Option<CancelToken>,
    /// Fail the drive with this error when the first text upload starts.
    pub fail_on_text: Option<SvnError>,
    /// Cancel this token right after the server accepted the MERGE.
    pub cancel_after_merge: Option<CancelToken>,
    /// Commit `(author, message, changes)` to the fake repository when the first text upload
    /// starts, so the commit in progress is out of date at MERGE.
    pub seed_on_text: Option<(String, String, Vec<FakeChange>)>,
    pub on_auth: Option<AuthHook>,
    /// One entry per `do_update` call, in order: break that drive with the error after this many
    /// `close_file` calls (as if the connection failed mid-stream); `None` lets it run.
    pub update_breaks: Mutex<std::collections::VecDeque<Option<(usize, SvnError)>>>,
    /// Revision asked for by every `do_update` call.
    pub update_revisions: Mutex<Vec<Option<Revnum>>>,
    /// Break every drive with the error when it is about to close this file (a server that
    /// always cuts the answer at the same point). Applies after `update_breaks` is used up.
    pub update_break_at: Mutex<Option<(String, SvnError)>>,
    /// One entry per `probe` call, in order: fail it with the error (`None`: the server
    /// answers). Once used up, `probe_default` decides.
    pub probe_results: Mutex<std::collections::VecDeque<Option<SvnError>>>,
    /// Answer of a probe once `probe_results` is used up (`None`: the server answers).
    pub probe_default: Mutex<Option<SvnError>>,
    /// Probes made.
    pub probes: std::sync::atomic::AtomicUsize,
    /// `reset_connections` calls.
    pub resets: std::sync::atomic::AtomicUsize,
}

impl RecRa {
    pub fn new(inner: FakeRaSession) -> Self {
        Self {
            inner,
            log: Arc::default(),
            record_only: false,
            next_rev: 2,
            cancel_on_text: None,
            fail_on_text: None,
            cancel_after_merge: None,
            seed_on_text: None,
            on_auth: None,
            update_breaks: Mutex::default(),
            update_revisions: Mutex::default(),
            update_break_at: Mutex::default(),
            probe_results: Mutex::default(),
            probe_default: Mutex::default(),
            probes: std::sync::atomic::AtomicUsize::new(0),
            resets: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn events(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn push(log: &Log, s: String) {
    log.lock().unwrap_or_else(PoisonError::into_inner).push(s);
}

struct RecEditor {
    log: Log,
    inner: Option<Box<dyn CommitEditor>>,
    observer: Arc<dyn CommitObserver>,
    next_rev: u64,
    cancel_on_text: Option<CancelToken>,
    fail_on_text: Option<SvnError>,
    cancel_after_merge: Option<CancelToken>,
    seed_on_text: Option<(String, String, Vec<FakeChange>)>,
    fake: FakeRaSession,
    dirs: HashMap<DirId, String>,
    files: HashMap<FileId, String>,
    counter: u32,
}

impl RecEditor {
    fn id(&mut self) -> u32 {
        self.counter += 1;
        self.counter
    }
}

fn name(copy: Option<(&str, Revnum)>) -> String {
    copy.map_or(String::new(), |(p, r)| format!(" copy={p}@{}", r.0))
}

#[async_trait]
impl CommitEditor for RecEditor {
    async fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId> {
        push(&self.log, format!("open_root {:?}", base_rev.map(|r| r.0)));
        let id = match &mut self.inner {
            Some(e) => e.open_root(base_rev).await?,
            None => DirId(self.id()),
        };
        self.dirs.insert(id, String::new());
        Ok(id)
    }

    async fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        push(&self.log, format!("add_dir {path}{}", name(copy_from)));
        let id = match &mut self.inner {
            Some(e) => e.add_directory(path, parent, copy_from).await?,
            None => DirId(self.id()),
        };
        self.dirs.insert(id, path.as_str().to_owned());
        Ok(id)
    }

    async fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        push(
            &self.log,
            format!("open_dir {path} @{:?}", base_rev.map(|r| r.0)),
        );
        let id = match &mut self.inner {
            Some(e) => e.open_directory(path, parent, base_rev).await?,
            None => DirId(self.id()),
        };
        self.dirs.insert(id, path.as_str().to_owned());
        Ok(id)
    }

    async fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        push(&self.log, format!("add_file {path}{}", name(copy_from)));
        let id = match &mut self.inner {
            Some(e) => e.add_file(path, parent, copy_from).await?,
            None => FileId(self.id()),
        };
        self.files.insert(id, path.as_str().to_owned());
        Ok(id)
    }

    async fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        push(
            &self.log,
            format!("open_file {path} @{:?}", base_rev.map(|r| r.0)),
        );
        let id = match &mut self.inner {
            Some(e) => e.open_file(path, parent, base_rev).await?,
            None => FileId(self.id()),
        };
        self.files.insert(id, path.as_str().to_owned());
        Ok(id)
    }

    async fn apply_text(
        &mut self,
        file: FileId,
        base: Option<ReplayableSource>,
        target: ReplayableSource,
        base_md5: Option<Checksum>,
        result_md5: Checksum,
    ) -> Result<()> {
        let path = self.files.get(&file).cloned().unwrap_or_default();
        let content = std::fs::read_to_string(&target.path).unwrap_or_default();
        push(
            &self.log,
            format!(
                "text {path} base={} bytes={:?}",
                base.is_some() && base_md5.is_some(),
                content
            ),
        );
        if let Some(c) = self.cancel_on_text.take() {
            c.cancel();
        }
        if let Some(e) = self.fail_on_text.take() {
            return Err(e);
        }
        if let Some((author, message, changes)) = self.seed_on_text.take() {
            self.fake
                .seed(&author, &message, &changes)
                .expect("concurrent commit");
        }
        match &mut self.inner {
            Some(e) => e.apply_text(file, base, target, base_md5, result_md5).await,
            None => Ok(()),
        }
    }

    async fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        push(
            &self.log,
            format!(
                "dir_prop {} {}={:?}",
                self.dirs.get(&dir).cloned().unwrap_or_default(),
                change.name,
                change
                    .value
                    .as_ref()
                    .map(|v| String::from_utf8_lossy(v).into_owned())
            ),
        );
        match &mut self.inner {
            Some(e) => e.change_dir_prop(dir, change).await,
            None => Ok(()),
        }
    }

    async fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        push(
            &self.log,
            format!(
                "file_prop {} {}={:?}",
                self.files.get(&file).cloned().unwrap_or_default(),
                change.name,
                change
                    .value
                    .as_ref()
                    .map(|v| String::from_utf8_lossy(v).into_owned())
            ),
        );
        match &mut self.inner {
            Some(e) => e.change_file_prop(file, change).await,
            None => Ok(()),
        }
    }

    async fn delete_entry(
        &mut self,
        path: &RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    ) -> Result<()> {
        push(&self.log, format!("delete {path} @{:?}", rev.map(|r| r.0)));
        match &mut self.inner {
            Some(e) => e.delete_entry(path, rev, parent).await,
            None => Ok(()),
        }
    }

    async fn close_file(&mut self, file: FileId) -> Result<()> {
        push(&self.log, "close_file".into());
        match &mut self.inner {
            Some(e) => e.close_file(file).await,
            None => Ok(()),
        }
    }

    async fn close_directory(&mut self, dir: DirId) -> Result<()> {
        push(&self.log, "close_dir".into());
        match &mut self.inner {
            Some(e) => e.close_directory(dir).await,
            None => Ok(()),
        }
    }

    async fn close_edit(self: Box<Self>) -> Result<CommitInfo> {
        let this = *self;
        push(&this.log, "close_edit".into());
        let merged = match this.inner {
            Some(e) => e.close_edit().await,
            None => {
                let info = CommitInfo {
                    revision: Revnum(this.next_rev),
                    date: Some(svn_types::Timestamp(1_760_000_000_000_000)),
                    author: Some("tester".into()),
                    post_commit_err: None,
                };
                this.observer.on_merge_sending("recorded-txn")?;
                this.observer.on_merged(&info)?;
                Ok(info)
            }
        };
        if merged.is_ok()
            && let Some(t) = &this.cancel_after_merge
        {
            t.cancel();
        }
        merged
    }

    async fn abort_edit(self: Box<Self>) -> Result<()> {
        let this = *self;
        push(&this.log, "abort_edit".into());
        match this.inner {
            Some(e) => e.abort_edit().await,
            None => Ok(()),
        }
    }
}

#[async_trait]
impl RaSession for RecRa {
    fn repos_root(&self) -> &RepoUrl {
        self.inner.repos_root()
    }
    fn uuid(&self) -> &str {
        self.inner.uuid()
    }
    fn session_url(&self) -> &RepoUrl {
        self.inner.session_url()
    }
    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()> {
        push(&self.log, "ensure_authenticated".into());
        if let Some(h) = &self.on_auth {
            h();
        }
        self.inner.ensure_authenticated(cancel).await
    }
    async fn probe(&self, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        self.probes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        push(&self.log, "probe".into());
        let next = self
            .probe_results
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let answer = match next {
            Some(a) => a,
            None => self
                .probe_default
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        };
        match answer {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    fn reset_connections(&self) {
        self.resets
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    async fn youngest_rev(&self, cancel: &CancelToken) -> Result<Revnum> {
        self.inner.youngest_rev(cancel).await
    }
    async fn check_path(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<NodeKind> {
        self.inner.check_path(path, rev, cancel).await
    }
    async fn stat(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Option<DirEntry>> {
        self.inner.stat(path, rev, cancel).await
    }
    async fn list(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<DirEntry>> {
        self.inner.list(path, rev, depth, cancel).await
    }
    async fn get_file(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancelToken,
    ) -> Result<FileMeta> {
        self.inner.get_file(path, rev, sink, cancel).await
    }
    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
        cancel: &CancelToken,
    ) -> Result<()> {
        self.inner.log(req, on_entry, cancel).await
    }
    async fn get_locks(
        &self,
        path: &RelPath,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<LockInfo>> {
        self.inner.get_locks(path, depth, cancel).await
    }
    async fn lock(
        &self,
        path: &RelPath,
        comment: Option<&str>,
        steal: bool,
        base_rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<LockInfo> {
        self.inner
            .lock(path, comment, steal, base_rev, cancel)
            .await
    }
    async fn unlock(
        &self,
        path: &RelPath,
        token: Option<&str>,
        break_lock: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.inner.unlock(path, token, break_lock, cancel).await
    }
    async fn file_revs(
        &self,
        path: &RelPath,
        start: Option<Revnum>,
        end: Option<Revnum>,
        handler: &mut dyn FileRevHandler,
        cancel: &CancelToken,
    ) -> Result<()> {
        self.inner
            .file_revs(path, start, end, handler, cancel)
            .await
    }
    async fn get_location_segments(
        &self,
        path: &RelPath,
        peg: Option<Revnum>,
        start: Option<Revnum>,
        end: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Vec<svn_ra_api::LocationSegment>> {
        self.inner
            .get_location_segments(path, peg, start, end, cancel)
            .await
    }
    async fn do_update(
        &self,
        req: UpdateRequest,
        report: ReportFn,
        editor: Box<dyn DeltaEditor>,
        cancel: &CancelToken,
    ) -> Result<()> {
        push(&self.log, format!("do_update rev={:?}", req.revision));
        self.update_revisions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(req.revision);
        let brk = self
            .update_breaks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .flatten();
        let at = self
            .update_break_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let editor: Box<dyn DeltaEditor> = match (brk, at) {
            (Some((after, error)), _) => Box::new(BreakingEditor {
                inner: editor,
                left: after,
                error,
                at: None,
                files: HashMap::new(),
            }),
            (None, Some((path, error))) => Box::new(BreakingEditor {
                inner: editor,
                left: usize::MAX,
                error,
                at: Some(path),
                files: HashMap::new(),
            }),
            (None, None) => editor,
        };
        self.inner.do_update(req, report, editor, cancel).await
    }
    async fn commit_editor(
        &self,
        message: &str,
        rev_props: &[PropChange],
        lock_tokens: &[(String, String)],
        keep_locks: bool,
        observer: Arc<dyn CommitObserver>,
        cancel: &CancelToken,
    ) -> Result<Box<dyn CommitEditor>> {
        push(
            &self.log,
            format!("commit_editor msg={message:?} tokens={lock_tokens:?} keep_locks={keep_locks}"),
        );
        let inner = if self.record_only {
            observer.on_txn_created("recorded-txn")?;
            None
        } else {
            Some(
                self.inner
                    .commit_editor(
                        message,
                        rev_props,
                        lock_tokens,
                        keep_locks,
                        observer.clone(),
                        cancel,
                    )
                    .await?,
            )
        };
        Ok(Box::new(RecEditor {
            log: self.log.clone(),
            inner,
            observer,
            next_rev: self.next_rev,
            cancel_on_text: self.cancel_on_text.clone(),
            fail_on_text: self.fail_on_text.clone(),
            cancel_after_merge: self.cancel_after_merge.clone(),
            seed_on_text: self.seed_on_text.clone(),
            fake: self.inner.clone(),
            dirs: HashMap::new(),
            files: HashMap::new(),
            counter: 0,
        }))
    }
    async fn abort_txn(&self, txn_name: &str) -> Result<()> {
        push(&self.log, format!("abort_txn {txn_name}"));
        self.inner.abort_txn(txn_name).await
    }
    async fn abort_txn_checked(&self, txn_name: &str, cancel: &CancelToken) -> Result<TxnAbort> {
        push(&self.log, format!("abort_txn_checked {txn_name}"));
        self.inner.abort_txn_checked(txn_name, cancel).await
    }
}

/// An error with a Subversion code the RA layer could not classify.
pub fn raw_error(svn_code: u32) -> SvnError {
    SvnError::new(ErrorCode::Internal, "server said no").with_svn_code(svn_code)
}

/// Editor that fails like a broken connection after `left` files were closed: the failing
/// call never reaches the working copy editor, which then gets `abort_edit`.
struct BreakingEditor {
    inner: Box<dyn DeltaEditor>,
    left: usize,
    error: SvnError,
    /// Also break at the `close_file` of this path.
    at: Option<String>,
    files: HashMap<FileId, String>,
}

impl BreakingEditor {
    fn check(&self) -> Result<()> {
        if self.left == 0 {
            return Err(self.error.clone());
        }
        Ok(())
    }
}

impl DeltaEditor for BreakingEditor {
    fn set_target_revision(&mut self, rev: Revnum) -> Result<()> {
        self.inner.set_target_revision(rev)
    }
    fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId> {
        self.inner.open_root(base_rev)
    }
    fn delete_entry(&mut self, path: &RelPath, rev: Option<Revnum>, parent: DirId) -> Result<()> {
        self.check()?;
        self.inner.delete_entry(path, rev, parent)
    }
    fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copyfrom: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        self.check()?;
        self.inner.add_directory(path, parent, copyfrom)
    }
    fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        self.check()?;
        self.inner.open_directory(path, parent, base_rev)
    }
    fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        self.check()?;
        self.inner.change_dir_prop(dir, change)
    }
    fn close_directory(&mut self, dir: DirId) -> Result<()> {
        self.check()?;
        self.inner.close_directory(dir)
    }
    fn absent_directory(&mut self, path: &RelPath, parent: DirId) -> Result<()> {
        self.check()?;
        self.inner.absent_directory(path, parent)
    }
    fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copyfrom: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        self.check()?;
        let id = self.inner.add_file(path, parent, copyfrom)?;
        self.files.insert(id, path.as_str().to_owned());
        Ok(id)
    }
    fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        self.check()?;
        let id = self.inner.open_file(path, parent, base_rev)?;
        self.files.insert(id, path.as_str().to_owned());
        Ok(id)
    }
    fn apply_textdelta(
        &mut self,
        file: FileId,
        base_checksum: Option<Checksum>,
    ) -> Result<Box<dyn svn_types::WindowSink>> {
        self.check()?;
        self.inner.apply_textdelta(file, base_checksum)
    }
    fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        self.check()?;
        self.inner.change_file_prop(file, change)
    }
    fn close_file(&mut self, file: FileId, text_checksum: Option<Checksum>) -> Result<()> {
        self.check()?;
        if self.at.is_some() && self.files.get(&file) == self.at.as_ref() {
            return Err(self.error.clone());
        }
        self.inner.close_file(file, text_checksum)?;
        self.left -= 1;
        Ok(())
    }
    fn absent_file(&mut self, path: &RelPath, parent: DirId) -> Result<()> {
        self.check()?;
        self.inner.absent_file(path, parent)
    }
    fn close_edit(&mut self) -> Result<()> {
        self.check()?;
        self.inner.close_edit()
    }
    fn abort_edit(&mut self) -> Result<()> {
        self.inner.abort_edit()
    }
}
