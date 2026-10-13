//! Commit editor of the fake repository: collects changes in a private transaction and
//! turns them into a new revision on `close_edit`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use svn_types::{
    CancelToken, ChangeAction, ChangedPath, Checksum, DirId, ErrorCode, FileId, NodeKind,
    PropChange, RelPath, RepoDisplayPath, RepoUrl, Result, Revnum, SvnError, Timestamp,
};

use super::repo::{FakeChange, Node, RepoState, Tree, apply_change};
use crate::commit::{CommitEditor, CommitInfo, CommitObserver, ReplayableSource};

pub(super) struct FakeCommitEditor {
    pub state: Arc<Mutex<RepoState>>,
    pub observer: Arc<dyn CommitObserver>,
    pub txn: String,
    pub author: String,
    pub message: String,
    pub lock_tokens: Vec<(String, String)>,
    pub keep_locks: bool,
    /// Fires when the caller cancels the commit; `abort_edit` still works afterwards.
    pub cancel: CancelToken,
    /// Working tree of the transaction, starting from HEAD at creation.
    pub tree: Tree,
    pub base_head: Revnum,
    pub changed: Vec<ChangedPath>,
    pub dirs: HashMap<DirId, RelPath>,
    pub files: HashMap<FileId, RelPath>,
    pub next_id: u32,
    pub touched: Vec<RelPath>,
    /// Repository root URL, to resolve copy sources.
    pub root: RepoUrl,
}

fn guard(state: &Mutex<RepoState>) -> MutexGuard<'_, RepoState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn err(code: ErrorCode, msg: impl Into<String>) -> SvnError {
    SvnError::new(code, msg)
}

impl FakeCommitEditor {
    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn dir_path(&self, d: DirId) -> Result<RelPath> {
        self.dirs
            .get(&d)
            .cloned()
            .ok_or_else(|| err(ErrorCode::Internal, "unknown directory handle"))
    }

    fn file_path(&self, f: FileId) -> Result<RelPath> {
        self.files
            .get(&f)
            .cloned()
            .ok_or_else(|| err(ErrorCode::Internal, "unknown file handle"))
    }

    /// Rejects edits to nodes changed after `base_rev` and to locks held without a token.
    fn check_open(&self, path: &RelPath, base_rev: Option<Revnum>) -> Result<()> {
        let node = self
            .tree
            .get(path)
            .ok_or_else(|| err(ErrorCode::PathNotFound, format!("{path} not found")))?;
        if let Some(b) = base_rev
            && node.created_rev > b
        {
            return Err(err(ErrorCode::OutOfDate, format!("{path} is out of date")));
        }
        Ok(())
    }

    fn check_lock(&self, path: &RelPath) -> Result<()> {
        let st = guard(&self.state);
        let Some(lock) = st.locks.get(path) else {
            return Ok(());
        };
        match self
            .lock_tokens
            .iter()
            .find(|(p, _)| p.trim_start_matches('/') == path.as_str())
        {
            Some((_, t)) if *t == lock.token => Ok(()),
            Some(_) => Err(err(
                ErrorCode::LockTokenInvalid,
                format!("lock token for {path} does not match"),
            )),
            None => Err(err(
                ErrorCode::LockedByOther,
                format!("{path} is locked by {}", lock.owner),
            )),
        }
    }

    fn apply(&mut self, change: FakeChange) -> Result<()> {
        let rev = Revnum(self.base_head.0 + 1);
        apply_change(&mut self.tree, rev, &change, &mut self.changed)
    }

    /// Adds `path` as a copy of the node named by the URL `from` at `rev`, with its subtree:
    /// the copy root is new in this revision, the copied children keep their last-changed
    /// revisions (they share the source's node revisions, as in FSFS).
    fn copy(&mut self, path: &RelPath, from: &str, rev: Revnum, kind: NodeKind) -> Result<()> {
        let new_rev = Revnum(self.base_head.0 + 1);
        let (src, nodes) = {
            let st = guard(&self.state);
            let (_, r) = st.revision(Some(rev))?;
            let src = r
                .tree
                .keys()
                .find(|k| {
                    self.root.join(k).is_ok_and(|u| {
                        u.as_url().as_str().trim_end_matches('/') == from.trim_end_matches('/')
                    })
                })
                .cloned()
                .ok_or_else(|| {
                    err(
                        ErrorCode::PathNotFound,
                        format!("copy source {from}@{} not found", rev.0),
                    )
                })?;
            let nodes: Vec<(RelPath, Node)> = r
                .tree
                .iter()
                .filter(|(k, _)| k.starts_with(&src))
                .map(|(k, n)| (k.clone(), n.clone()))
                .collect();
            (src, nodes)
        };
        if nodes.first().map(|(_, n)| n.kind()) != Some(kind) {
            return Err(err(
                ErrorCode::Internal,
                format!("copy source {from} is not a {kind:?}"),
            ));
        }
        for (k, mut n) in nodes {
            let suffix = k
                .as_str()
                .strip_prefix(src.as_str())
                .unwrap_or("")
                .trim_start_matches('/');
            let target = if suffix.is_empty() {
                n.created_rev = new_rev;
                path.clone()
            } else {
                RelPath::new(&format!("{path}/{suffix}"))?
            };
            self.tree.insert(target, n);
        }
        self.changed.push(ChangedPath {
            path: RepoDisplayPath::from_rel(path),
            action: ChangeAction::Added,
            kind,
            copy_from: Some((RepoDisplayPath::from_rel(&src), rev)),
            text_mods: false,
            prop_mods: false,
        });
        let mut parent = path.parent();
        while let Some(p) = parent {
            if let Some(n) = self.tree.get_mut(&p) {
                n.created_rev = new_rev;
            }
            parent = p.parent();
        }
        Ok(())
    }

    fn require_new(&self, path: &RelPath) -> Result<()> {
        if self.tree.contains_key(path) {
            return Err(err(ErrorCode::Internal, format!("{path} already exists")));
        }
        Ok(())
    }
}

#[async_trait]
impl CommitEditor for FakeCommitEditor {
    async fn open_root(&mut self, base_rev: Option<Revnum>) -> Result<DirId> {
        self.cancel.check()?;
        self.check_open(&RelPath::root(), base_rev)?;
        let id = DirId(self.id());
        self.dirs.insert(id, RelPath::root());
        Ok(id)
    }

    async fn add_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<DirId> {
        self.cancel.check()?;
        self.dir_path(parent)?;
        self.require_new(path)?;
        match copy_from {
            Some((from, rev)) => self.copy(path, from, rev, NodeKind::Dir)?,
            None => self.apply(FakeChange::PutDir(path.clone()))?,
        }
        self.touched.push(path.clone());
        let id = DirId(self.id());
        self.dirs.insert(id, path.clone());
        Ok(id)
    }

    async fn open_directory(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<DirId> {
        self.cancel.check()?;
        self.dir_path(parent)?;
        self.check_open(path, base_rev)?;
        let id = DirId(self.id());
        self.dirs.insert(id, path.clone());
        Ok(id)
    }

    async fn add_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        copy_from: Option<(&str, Revnum)>,
    ) -> Result<FileId> {
        self.cancel.check()?;
        self.dir_path(parent)?;
        self.require_new(path)?;
        self.check_lock(path)?;
        match copy_from {
            Some((from, rev)) => self.copy(path, from, rev, NodeKind::File)?,
            None => self.apply(FakeChange::PutFile(path.clone(), Bytes::new()))?,
        }
        self.touched.push(path.clone());
        let id = FileId(self.id());
        self.files.insert(id, path.clone());
        Ok(id)
    }

    async fn open_file(
        &mut self,
        path: &RelPath,
        parent: DirId,
        base_rev: Option<Revnum>,
    ) -> Result<FileId> {
        self.cancel.check()?;
        self.dir_path(parent)?;
        self.check_open(path, base_rev)?;
        self.check_lock(path)?;
        let id = FileId(self.id());
        self.files.insert(id, path.clone());
        Ok(id)
    }

    async fn apply_text(
        &mut self,
        file: FileId,
        _base: Option<ReplayableSource>,
        target: ReplayableSource,
        _base_md5: Option<Checksum>,
        _result_md5: Checksum,
    ) -> Result<()> {
        self.cancel.check()?;
        let path = self.file_path(file)?;
        let content = tokio::fs::read(&target.path)
            .await
            .map_err(|e| SvnError::from_io(format!("read {}", target.path.display()), &e))?;
        // The token may have fired while the read was pending: do not touch the transaction.
        self.cancel.check()?;
        self.apply(FakeChange::PutFile(path.clone(), Bytes::from(content)))?;
        self.touched.push(path);
        Ok(())
    }

    async fn change_dir_prop(&mut self, dir: DirId, change: PropChange) -> Result<()> {
        self.cancel.check()?;
        let path = self.dir_path(dir)?;
        self.apply(FakeChange::SetProp(path.clone(), change))?;
        self.touched.push(path);
        Ok(())
    }

    async fn change_file_prop(&mut self, file: FileId, change: PropChange) -> Result<()> {
        self.cancel.check()?;
        let path = self.file_path(file)?;
        self.apply(FakeChange::SetProp(path.clone(), change))?;
        self.touched.push(path);
        Ok(())
    }

    async fn delete_entry(
        &mut self,
        path: &RelPath,
        rev: Option<Revnum>,
        parent: DirId,
    ) -> Result<()> {
        self.cancel.check()?;
        self.dir_path(parent)?;
        self.check_open(path, rev)?;
        self.check_lock(path)?;
        self.apply(FakeChange::Delete(path.clone()))?;
        self.touched.push(path.clone());
        Ok(())
    }

    async fn close_file(&mut self, file: FileId) -> Result<()> {
        self.cancel.check()?;
        self.file_path(file).map(|_| ())
    }

    async fn close_directory(&mut self, dir: DirId) -> Result<()> {
        self.cancel.check()?;
        self.dir_path(dir).map(|_| ())
    }

    async fn close_edit(self: Box<Self>) -> Result<CommitInfo> {
        let this = *self;
        if let Err(e) = this.cancel.check() {
            // Cancelled before the merge: delete the transaction instead of committing.
            guard(&this.state).txns.remove(&this.txn);
            return Err(e);
        }
        let info = {
            let mut st = guard(&this.state);
            if !st.txns.remove(&this.txn) {
                return Err(err(ErrorCode::Internal, "transaction no longer exists"));
            }
            // The fake has no merge logic: a commit that raced with another one is out of date.
            if st.youngest() != this.base_head {
                return Err(err(
                    ErrorCode::OutOfDate,
                    "repository changed while the commit was in progress",
                ));
            }
            let rev = Revnum(st.revisions.len() as u64);
            // Edits were stamped with base_head + 1, which is exactly `rev` here.
            let tree = this.tree.clone();
            if !this.keep_locks {
                for p in &this.touched {
                    if let Some(l) = st.locks.get(p)
                        && this.lock_tokens.iter().any(|(_, t)| *t == l.token)
                    {
                        st.locks.remove(p);
                    }
                }
            }
            st.install(
                rev,
                tree,
                Some(this.author.clone()),
                Some(this.message.clone()),
                this.changed.clone(),
            );
            CommitInfo {
                revision: rev,
                date: Some(Timestamp(
                    1_760_000_000_000_000 + (rev.0 as i64) * 1_000_000,
                )),
                author: Some(this.author.clone()),
                post_commit_err: None,
            }
        };
        if let Err(e) = this.observer.on_merged(&info) {
            let mut e2 = SvnError::new(
                e.code,
                format!(
                    "commit r{} was merged but the observer failed: {}",
                    info.revision.0, e.message
                ),
            );
            e2.svn_code = e.svn_code;
            return Err(e2);
        }
        Ok(info)
    }

    async fn abort_edit(self: Box<Self>) -> Result<()> {
        guard(&self.state).txns.remove(&self.txn);
        Ok(())
    }
}
