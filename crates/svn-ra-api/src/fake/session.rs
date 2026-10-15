//! `RaSession` implementation over the in-memory repository.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use secrecy::ExposeSecret;
use svn_types::{
    CancelToken, Checksum, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind,
    OriginKey, PropChange, RelPath, RepoDisplayPath, RepoUrl, Result, Revnum, SvnError,
    TxDeltaWindow,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::commit_editor::FakeCommitEditor;
use super::repo::{FakeChange, FakeMergeFault, NodeData, RepoState, children};
use super::update_drive::drive_update;
use crate::auth::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CredentialRequest, PromptReason,
};
use crate::commit::{CommitEditor, CommitObserver};
use crate::session::{
    FileMeta, FileRev, FileRevHandler, LogRequest, RaSession, ReportFn, TxnAbort, UpdateRequest,
};

/// Credentials the fake server accepts.
struct Required {
    username: String,
    password: String,
    prompter: Option<Arc<dyn AuthPrompter>>,
}

/// In-memory repository exposing the full [`RaSession`] contract, including commits that
/// create new revisions. Cheap to clone: clones share the repository.
#[derive(Clone)]
pub struct FakeRaSession {
    root: RepoUrl,
    /// URL the session is rooted at (the root unless made with [`Self::at`]).
    session: RepoUrl,
    /// `session` relative to `root`; every path argument is relative to it.
    session_rel: RelPath,
    uuid: String,
    user: String,
    pub(super) state: Arc<Mutex<RepoState>>,
    max_open_files: usize,
    required: Option<Arc<Required>>,
    cert_check: Option<(CertInfo, Arc<dyn AuthPrompter>)>,
    authenticated: Arc<AtomicBool>,
    lock_counter: Arc<AtomicU64>,
    copies: bool,
    /// Listings carry the locks of their entries (`false`: like a DAV server).
    listed_locks: bool,
}

impl FakeRaSession {
    /// Creates an empty repository (revision 0) reachable at `url`.
    pub fn new(url: &str, uuid: &str) -> Result<Self> {
        let root = RepoUrl::parse(url)?;
        Ok(Self {
            session: root.clone(),
            session_rel: RelPath::root(),
            root,
            uuid: uuid.to_owned(),
            user: "tester".to_owned(),
            state: Arc::new(Mutex::new(RepoState::new())),
            max_open_files: 4,
            required: None,
            cert_check: None,
            authenticated: Arc::new(AtomicBool::new(false)),
            lock_counter: Arc::new(AtomicU64::new(1)),
            copies: true,
            listed_locks: true,
        })
    }

    /// A session on the same repository rooted at `url` (at or below the repository root, on
    /// the same origin, otherwise `UnsafePath`), like a session opened at that URL: every path
    /// argument and the commit editor are relative to it. Clones share the repository.
    pub fn at(&self, url: &RepoUrl) -> Result<Self> {
        let session_rel = super::prefixed::rel_of(&self.root, url)?;
        Ok(Self {
            session: self.root.join(&session_rel)?,
            session_rel,
            ..self.clone()
        })
    }

    /// Repository-relative path of a session-relative one.
    fn abs(&self, path: &RelPath) -> Result<RelPath> {
        super::prefixed::join(&self.session_rel, path)
    }

    /// Sets how many files an update keeps open before closing the oldest (default 4).
    #[must_use]
    pub fn with_max_open_files(mut self, n: usize) -> Self {
        self.max_open_files = n;
        self
    }

    /// Makes commit editors from this session refuse copies with `ServerUnsupported`, like a
    /// server without copy support.
    #[must_use]
    pub fn without_copies(mut self) -> Self {
        self.copies = false;
        self
    }

    /// Makes listings carry no locks, like mod_dav_svn (locks come from `get_locks` only).
    #[must_use]
    pub fn without_listed_locks(mut self) -> Self {
        self.listed_locks = false;
        self
    }

    /// Sets the user name used for commits and locks.
    #[must_use]
    pub fn with_user(mut self, user: &str) -> Self {
        self.user = user.to_owned();
        self
    }

    /// Makes `ensure_authenticated` require these Basic credentials, asked through `prompter`.
    #[must_use]
    pub fn with_required_auth(
        mut self,
        username: &str,
        password: &str,
        prompter: Option<Arc<dyn AuthPrompter>>,
    ) -> Self {
        self.required = Some(Arc::new(Required {
            username: username.to_owned(),
            password: password.to_owned(),
            prompter,
        }));
        self
    }

    /// Makes `ensure_authenticated` first ask `prompter` to decide about `cert`, like the
    /// transport does after a failed TLS verification. The fake checks the decision against the
    /// failure (`CertDecision::is_valid_for`) and refuses with `TlsUntrusted` unless the
    /// decision is valid and not `Reject`.
    #[must_use]
    pub fn with_certificate_check(
        mut self,
        cert: CertInfo,
        prompter: Arc<dyn AuthPrompter>,
    ) -> Self {
        self.cert_check = Some((cert, prompter));
        self
    }

    /// Seeds the repository with a revision built from `changes`.
    pub fn seed(&self, author: &str, message: &str, changes: &[FakeChange]) -> Result<Revnum> {
        self.guard().commit_changes(author, message, changes)
    }

    /// Makes the next commit's `close_edit` lose the `MERGE` answer as `fault` describes.
    pub fn fail_next_merge(&self, fault: FakeMergeFault) {
        self.guard().merge_fault = Some(fault);
    }

    /// Makes every `abort_txn_checked` answer [`TxnAbort::Unknown`] with `reason` (a 5xx to the
    /// `DELETE`, such as svn 1.15's "Transaction cleanup failed"), without touching the
    /// transactions; `None` restores normal answers.
    pub fn fail_txn_deletes(&self, reason: Option<&str>) {
        self.guard().txn_delete_fault = reason.map(str::to_owned);
    }

    /// Names of commit transactions that are still open.
    pub fn open_txns(&self) -> Vec<String> {
        let mut v: Vec<String> = self.guard().txns.iter().cloned().collect();
        v.sort();
        v
    }

    fn guard(&self) -> MutexGuard<'_, RepoState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn origin(&self) -> OriginKey {
        let u = self.root.as_url();
        OriginKey {
            scheme: u.scheme().to_owned(),
            host: u.host_str().unwrap_or("").to_owned(),
            port: u.port_or_known_default().unwrap_or(0),
            realm: format!("<{}> fake", self.uuid),
        }
    }
}

fn not_found(path: &RelPath) -> SvnError {
    SvnError::new(ErrorCode::PathNotFound, format!("path not found: {path}"))
}

#[async_trait]
impl RaSession for FakeRaSession {
    fn repos_root(&self) -> &RepoUrl {
        &self.root
    }

    fn uuid(&self) -> &str {
        &self.uuid
    }

    fn reparented(&self, url: &RepoUrl) -> Result<Box<dyn RaSession>> {
        Ok(Box::new(self.at(url)?))
    }
    fn session_url(&self) -> &RepoUrl {
        &self.session
    }

    fn authenticated_user(&self) -> Option<String> {
        Some(self.user.clone())
    }

    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
        if let Some((cert, prompter)) = &self.cert_check {
            let decision = prompter.confirm_certificate(cert).await;
            // The prompt can take a long time: honor cancellation before acting on the answer.
            cancel.check()?;
            // The prompter (the UI) is not a trust boundary: validate against the real failure.
            if decision == CertDecision::Reject || !decision.is_valid_for(&cert.failure) {
                return Err(SvnError::new(
                    ErrorCode::TlsUntrusted,
                    "certificate not trusted",
                ));
            }
        }
        let Some(req) = &self.required else {
            return Ok(());
        };
        if self.authenticated.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(prompter) = &req.prompter else {
            return Err(SvnError::new(
                ErrorCode::AuthRequired,
                "authentication required",
            ));
        };
        let origin = self.origin();
        for attempt in 0..3u32 {
            cancel.check()?;
            let reason = if attempt == 0 {
                PromptReason::Initial
            } else {
                PromptReason::Rejected
            };
            let request =
                CredentialRequest::new(origin.clone(), attempt, vec![AuthMethod::Basic], reason);
            let Some(creds) = prompter.credentials(&request).await else {
                return Err(SvnError::new(
                    ErrorCode::Cancelled,
                    "authentication cancelled",
                ));
            };
            if creds.username == req.username && creds.password.expose_secret() == req.password {
                self.authenticated.store(true, Ordering::SeqCst);
                return Ok(());
            }
        }
        Err(SvnError::new(
            ErrorCode::AuthRequired,
            "authentication failed",
        ))
    }

    async fn youngest_rev(&self, cancel: &CancelToken) -> Result<Revnum> {
        cancel.check()?;
        Ok(self.guard().youngest())
    }

    async fn check_path(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<NodeKind> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let st = self.guard();
        let (_, rv) = st.revision(rev)?;
        Ok(rv.tree.get(path).map_or(NodeKind::None, |n| n.kind()))
    }

    async fn stat(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Option<DirEntry>> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let st = self.guard();
        let (_, rv) = st.revision(rev)?;
        Ok(RepoState::entry(&rv.tree, path, st.locks.get(path)))
    }

    async fn list(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<DirEntry>> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let st = self.guard();
        let (_, rv) = st.revision(rev)?;
        match rv.tree.get(path).map(|n| &n.data) {
            Some(NodeData::Dir) => {}
            Some(NodeData::File(_)) => {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("{path} is not a directory"),
                ));
            }
            None => return Err(not_found(path)),
        }
        let mut out = Vec::new();
        match depth {
            Depth::Empty | Depth::Exclude | Depth::Unknown => {}
            Depth::Files | Depth::Immediates => {
                for (p, n) in children(&rv.tree, path) {
                    if depth == Depth::Immediates || n.kind() == NodeKind::File {
                        out.extend(RepoState::entry(&rv.tree, p, st.locks.get(p)));
                    }
                }
            }
            Depth::Infinity => {
                for p in rv
                    .tree
                    .keys()
                    .filter(|p| !p.is_root() && p.starts_with(path) && *p != path)
                {
                    if let Some(mut e) = RepoState::entry(&rv.tree, p, st.locks.get(p)) {
                        // Below the first level the name is the path relative to `path`.
                        e.name = p
                            .as_str()
                            .get(path.as_str().len()..)
                            .unwrap_or("")
                            .trim_start_matches('/')
                            .to_owned();
                        out.push(e);
                    }
                }
            }
        }
        if !self.listed_locks {
            for e in &mut out {
                e.lock = None;
            }
        }
        Ok(out)
    }

    async fn get_file(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancelToken,
    ) -> Result<FileMeta> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let (rev, content, props) = {
            let st = self.guard();
            let (r, rv) = st.revision(rev)?;
            let node = rv.tree.get(path).ok_or_else(|| not_found(path))?;
            let NodeData::File(content) = &node.data else {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("{path} is not a file"),
                ));
            };
            let props = node
                .props
                .iter()
                .map(|(k, v)| PropChange {
                    name: k.clone(),
                    value: Some(v.clone()),
                })
                .collect::<Vec<_>>();
            (r, content.clone(), props)
        };
        sink.write_all(&content)
            .await
            .map_err(|e| SvnError::from_io("write file content", &e))?;
        let md5 = {
            use md5::Digest as _;
            Checksum::Md5(md5::Md5::digest(&content).into())
        };
        // The fake's own store is authoritative: its content is verified by construction.
        Ok(FileMeta {
            rev,
            props,
            md5,
            verified: true,
        })
    }

    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
        cancel: &CancelToken,
    ) -> Result<()> {
        let mut paths = req
            .paths
            .iter()
            .map(|p| self.abs(p))
            .collect::<Result<Vec<_>>>()?;
        if paths.is_empty() && !self.session_rel.is_root() {
            paths.push(self.session_rel.clone());
        }
        let req = &LogRequest {
            paths,
            ..req.clone()
        };
        let entries: Vec<LogEntry> = {
            let st = self.guard();
            let start = req
                .start
                .unwrap_or_else(|| st.youngest())
                .0
                .min(st.youngest().0);
            let end = req.end.map_or(1, |r| r.0.max(1));
            let mut out = Vec::new();
            for n in (end..=start).rev() {
                let Some(rv) = usize::try_from(n).ok().and_then(|i| st.revisions.get(i)) else {
                    continue;
                };
                let touched = req.paths.is_empty()
                    || req.paths.iter().any(|p| {
                        p.is_root()
                            || rv.changed.iter().any(|c| {
                                c.path
                                    .as_str()
                                    .trim_start_matches('/')
                                    .starts_with(p.as_str())
                            })
                    });
                if !touched {
                    continue;
                }
                out.push(LogEntry {
                    rev: Revnum(n),
                    author: rv.author.clone(),
                    date: Some(rv.date),
                    message: rv.message.clone(),
                    changed_paths: if req.discover_changed_paths {
                        rv.changed.clone()
                    } else {
                        Vec::new()
                    },
                    has_children: false,
                });
                if req.limit.is_some_and(|l| out.len() >= l as usize) {
                    break;
                }
            }
            out
        };
        for e in entries {
            cancel.check()?;
            on_entry(e)?;
        }
        Ok(())
    }

    async fn get_locks(
        &self,
        path: &RelPath,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<LockInfo>> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let st = self.guard();
        let mut v: Vec<LockInfo> = st
            .locks
            .iter()
            .filter(|(p, _)| match depth {
                Depth::Empty => *p == path,
                Depth::Files | Depth::Immediates => *p == path || p.parent().as_ref() == Some(path),
                _ => p.starts_with(path),
            })
            .map(|(_, l)| l.clone())
            .collect();
        v.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
        Ok(v)
    }

    async fn lock(
        &self,
        path: &RelPath,
        comment: Option<&str>,
        steal: bool,
        base_rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<LockInfo> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let mut st = self.guard();
        let head = st
            .revisions
            .last()
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "no revisions"))?;
        let node = head.tree.get(path).ok_or_else(|| not_found(path))?;
        if node.kind() != NodeKind::File {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("{path} is not a file"),
            ));
        }
        if base_rev.is_some_and(|b| node.created_rev > b) {
            return Err(SvnError::new(
                ErrorCode::OutOfDate,
                format!("{path} is out of date"),
            ));
        }
        if st.locks.contains_key(path) && !steal {
            return Err(SvnError::new(
                ErrorCode::LockedByOther,
                format!("{path} is already locked"),
            ));
        }
        let n = self.lock_counter.fetch_add(1, Ordering::SeqCst);
        let lock = LockInfo {
            path: RepoDisplayPath::from_rel(path),
            token: format!("opaquelocktoken:{}-{n}", self.uuid),
            owner: self.user.clone(),
            comment: comment.map(str::to_owned),
            created: svn_types::Timestamp(1_760_000_000_000_000 + n as i64),
            expires: None,
        };
        st.locks.insert(path.clone(), lock.clone());
        Ok(lock)
    }

    async fn unlock(
        &self,
        path: &RelPath,
        token: Option<&str>,
        break_lock: bool,
        cancel: &CancelToken,
    ) -> Result<()> {
        cancel.check()?;
        let path = &self.abs(path)?;
        let mut st = self.guard();
        let Some(existing) = st.locks.get(path) else {
            return Err(SvnError::new(
                ErrorCode::LockTokenInvalid,
                format!("{path} is not locked"),
            ));
        };
        if !break_lock && token != Some(existing.token.as_str()) {
            return Err(SvnError::new(
                ErrorCode::LockTokenInvalid,
                "lock token does not match",
            ));
        }
        st.locks.remove(path);
        Ok(())
    }

    async fn file_revs(
        &self,
        path: &RelPath,
        start: Option<Revnum>,
        end: Option<Revnum>,
        handler: &mut dyn FileRevHandler,
        cancel: &CancelToken,
    ) -> Result<()> {
        let path = &self.abs(path)?;
        let items: Vec<(FileRev, Bytes)> = {
            let st = self.guard();
            let lo = start.map_or(1, |r| r.0.max(1));
            let hi = end.unwrap_or_else(|| st.youngest()).0.min(st.youngest().0);
            let abs = RepoDisplayPath::from_rel(path);
            let mut v = Vec::new();
            for n in lo..=hi {
                let Some(rv) = usize::try_from(n).ok().and_then(|i| st.revisions.get(i)) else {
                    continue;
                };
                let touched = rv.changed.iter().any(|c| c.path == abs && c.text_mods);
                let content = match rv.tree.get(path).map(|n| &n.data) {
                    Some(NodeData::File(b)) if touched => b.clone(),
                    _ => continue,
                };
                let mut rev_props = Vec::new();
                if let Some(a) = &rv.author {
                    rev_props.push(PropChange {
                        name: "svn:author".into(),
                        value: Some(Bytes::from(a.clone())),
                    });
                }
                if let Some(m) = &rv.message {
                    rev_props.push(PropChange {
                        name: "svn:log".into(),
                        value: Some(Bytes::from(m.clone())),
                    });
                }
                v.push((
                    FileRev {
                        path: abs.clone(),
                        rev: Revnum(n),
                        rev_props,
                        merged: false,
                    },
                    content,
                ));
            }
            v
        };
        for (fr, content) in items {
            cancel.check()?;
            if let Some(mut sink) = handler.revision(fr)? {
                if !content.is_empty() {
                    sink.window(Some(&TxDeltaWindow::full_text(content)))?;
                }
                sink.window(None)?;
            }
        }
        Ok(())
    }

    async fn do_update(
        &self,
        req: UpdateRequest,
        report: ReportFn,
        editor: Box<dyn DeltaEditor>,
        cancel: &CancelToken,
    ) -> Result<()> {
        drive_update(
            &self.state,
            super::update_drive::SessionAt {
                repos_root: &self.root,
                session_rel: &self.session_rel,
            },
            self.max_open_files,
            req,
            report,
            editor,
            cancel,
        )
        .await
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
        cancel.check()?;
        let lock_tokens = &super::prefixed::lock_tokens(&self.session_rel, lock_tokens)?;
        let (txn, tree, base_head) = {
            let mut st = self.guard();
            let txn = format!("{}-fake", st.next_txn);
            st.next_txn += 1;
            st.txns.insert(txn.clone());
            let head = st
                .revisions
                .last()
                .ok_or_else(|| SvnError::new(ErrorCode::Internal, "no revisions"))?;
            (txn, head.tree.clone(), st.youngest())
        };
        if let Err(e) = observer.on_txn_created(&txn) {
            // The journal refused the transaction: delete it and send nothing.
            self.guard().txns.remove(&txn);
            return Err(e);
        }
        let author = rev_props
            .iter()
            .find(|p| p.name == "svn:author")
            .and_then(|p| p.value.as_ref())
            .map_or_else(
                || self.user.clone(),
                |v| String::from_utf8_lossy(v).into_owned(),
            );
        let editor: Box<dyn CommitEditor> = Box::new(FakeCommitEditor {
            state: Arc::clone(&self.state),
            observer,
            txn,
            author,
            message: message.to_owned(),
            lock_tokens: lock_tokens.to_vec(),
            keep_locks,
            cancel: cancel.clone(),
            tree,
            base_head,
            changed: Vec::new(),
            dir_bases: HashMap::new(),
            dirs: HashMap::new(),
            files: HashMap::new(),
            next_id: 0,
            touched: Vec::new(),
            root: self.root.clone(),
            copies: self.copies,
        });
        Ok(super::prefixed::wrap(editor, &self.session_rel))
    }

    async fn abort_txn(&self, txn_name: &str) -> Result<()> {
        self.guard().txns.remove(txn_name);
        Ok(())
    }

    async fn abort_txn_checked(&self, txn_name: &str, cancel: &CancelToken) -> Result<TxnAbort> {
        cancel.check()?;
        let mut state = self.guard();
        if let Some(reason) = &state.txn_delete_fault {
            return Ok(TxnAbort::Unknown(reason.clone()));
        }
        Ok(if state.txns.remove(txn_name) {
            TxnAbort::Deleted
        } else {
            TxnAbort::Gone
        })
    }
}
