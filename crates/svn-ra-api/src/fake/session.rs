//! `RaSession` implementation over the in-memory repository.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use secrecy::ExposeSecret;
use svn_types::{
    CancelToken, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind, OriginKey,
    PropChange, RelPath, RepoUrl, Result, Revnum, SvnError, TxDeltaWindow,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::commit_editor::FakeCommitEditor;
use super::repo::{FakeChange, NodeData, RepoState, children};
use super::update_drive::drive_update;
use crate::auth::{AuthMethod, AuthPrompter};
use crate::commit::{CommitEditor, CommitObserver};
use crate::session::{
    FileMeta, FileRev, FileRevHandler, LogRequest, RaSession, ReportFn, UpdateRequest,
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
    uuid: String,
    user: String,
    pub(super) state: Arc<Mutex<RepoState>>,
    max_open_files: usize,
    required: Option<Arc<Required>>,
    authenticated: Arc<AtomicBool>,
    lock_counter: Arc<AtomicU64>,
}

impl FakeRaSession {
    /// Creates an empty repository (revision 0) reachable at `url`.
    pub fn new(url: &str, uuid: &str) -> Result<Self> {
        Ok(Self {
            root: RepoUrl::parse(url)?,
            uuid: uuid.to_owned(),
            user: "tester".to_owned(),
            state: Arc::new(Mutex::new(RepoState::new())),
            max_open_files: 4,
            required: None,
            authenticated: Arc::new(AtomicBool::new(false)),
            lock_counter: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Sets how many files an update keeps open before closing the oldest (default 4).
    #[must_use]
    pub fn with_max_open_files(mut self, n: usize) -> Self {
        self.max_open_files = n;
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

    /// Seeds the repository with a revision built from `changes`.
    pub fn seed(&self, author: &str, message: &str, changes: &[FakeChange]) -> Result<Revnum> {
        self.guard().commit_changes(author, message, changes)
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

    fn session_url(&self) -> &RepoUrl {
        &self.root
    }

    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<()> {
        cancel.check()?;
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
        for attempt in 0..3u8 {
            cancel.check()?;
            let Some(creds) = prompter
                .credentials(&origin, attempt, &[AuthMethod::Basic])
                .await
            else {
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
        Ok(FileMeta { rev, props })
    }

    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
        cancel: &CancelToken,
    ) -> Result<()> {
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
                            || rv
                                .changed
                                .iter()
                                .any(|c| c.path.trim_start_matches('/').starts_with(p.as_str()))
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
        v.sort_by(|a, b| a.path.cmp(&b.path));
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
            path: format!("/{path}"),
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
        let items: Vec<(FileRev, Bytes)> = {
            let st = self.guard();
            let lo = start.map_or(1, |r| r.0.max(1));
            let hi = end.unwrap_or_else(|| st.youngest()).0.min(st.youngest().0);
            let abs = format!("/{path}");
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
            &self.root,
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
    ) -> Result<Box<dyn CommitEditor>> {
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
        observer.on_txn_created(&txn);
        let author = rev_props
            .iter()
            .find(|p| p.name == "svn:author")
            .and_then(|p| p.value.as_ref())
            .map_or_else(
                || self.user.clone(),
                |v| String::from_utf8_lossy(v).into_owned(),
            );
        Ok(Box::new(FakeCommitEditor {
            state: Arc::clone(&self.state),
            observer,
            txn,
            author,
            message: message.to_owned(),
            lock_tokens: lock_tokens.to_vec(),
            keep_locks,
            tree,
            base_head,
            changed: Vec::new(),
            dirs: HashMap::new(),
            files: HashMap::new(),
            next_id: 0,
            touched: Vec::new(),
        }))
    }

    async fn abort_txn(&self, txn_name: &str) -> Result<()> {
        self.guard().txns.remove(txn_name);
        Ok(())
    }
}
