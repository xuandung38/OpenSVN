//! In-memory working copy model behind [`FakeEngine`](super::FakeEngine).
//!
//! A working copy is a tree of versioned and unversioned nodes with a pristine ("base") and a
//! working version of each file. Nothing touches the disk, so the UI can run on fake data.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bytes::Bytes;
use opensvn_ipc::{FileKind, FileStatus, LockInfo, NodeStatus, PropStatus, Rev, Size, WcFormat};
use svn_types::{ErrorCode, RelPath, SvnError};

use crate::engine::facade::{StatusQuery, WcInfo};

/// Scheduling state of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sched {
    /// Versioned, no pending add or delete.
    Normal,
    /// Scheduled for addition.
    Added,
    /// Scheduled for deletion.
    Deleted,
    /// Present in the working tree only.
    Unversioned,
}

/// One file or directory.
#[derive(Debug, Clone)]
pub struct FakeNode {
    pub kind: FileKind,
    pub sched: Sched,
    /// Pristine content (files).
    pub base: Option<Bytes>,
    /// Working content; `None` means the file is missing on disk.
    pub work: Option<Bytes>,
    /// Revision the node was last updated to.
    pub rev: u64,
    pub props: BTreeMap<String, String>,
    pub base_props: BTreeMap<String, String>,
    pub conflicted: bool,
    pub tree_conflict: bool,
    pub changelist: Option<String>,
    pub lock: Option<LockInfo>,
    pub moved_from: Option<String>,
    pub moved_to: Option<String>,
}

impl FakeNode {
    /// Versioned file at `rev` whose working copy equals the pristine.
    pub fn file(content: &[u8], rev: u64) -> Self {
        Self {
            kind: FileKind::File,
            sched: Sched::Normal,
            base: Some(Bytes::copy_from_slice(content)),
            work: Some(Bytes::copy_from_slice(content)),
            rev,
            props: BTreeMap::new(),
            base_props: BTreeMap::new(),
            conflicted: false,
            tree_conflict: false,
            changelist: None,
            lock: None,
            moved_from: None,
            moved_to: None,
        }
    }

    /// Versioned directory at `rev`.
    pub fn dir(rev: u64) -> Self {
        Self {
            kind: FileKind::Dir,
            base: None,
            work: None,
            ..Self::file(b"", rev)
        }
    }

    /// Unversioned file.
    pub fn unversioned_file(content: &[u8]) -> Self {
        Self {
            sched: Sched::Unversioned,
            base: None,
            ..Self::file(content, 0)
        }
    }

    /// Whether the node is part of the repository tree (not unversioned, not scheduled away).
    pub fn is_versioned(&self) -> bool {
        self.sched != Sched::Unversioned
    }

    fn is_dir(&self) -> bool {
        self.kind == FileKind::Dir
    }
}

/// A working copy.
#[derive(Debug, Clone)]
pub struct FakeWc {
    pub root: PathBuf,
    pub repo_root_url: String,
    pub uuid: String,
    /// Root path inside the repository, `/`-prefixed, no trailing slash (`""` = repository root).
    pub path_in_repo: String,
    pub format: WcFormat,
    pub store_pristine: bool,
    pub nodes: BTreeMap<RelPath, FakeNode>,
    pub ignores: Vec<String>,
}

fn not_found(path: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("{path} is not in the working copy"),
    )
}

/// Parses a UI path, rejecting traversal and absolute paths.
pub fn rel(path: &str) -> Result<RelPath, SvnError> {
    RelPath::new(path)
}

/// Glob-lite matching for ignore rules: `*.ext`, `prefix*` or an exact name.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post)
        }
    }
}

impl FakeWc {
    /// Node at `path` or `PathNotFound`.
    pub fn node(&self, path: &RelPath) -> Result<&FakeNode, SvnError> {
        self.nodes.get(path).ok_or_else(|| not_found(path))
    }

    /// Mutable node at `path` or `PathNotFound`.
    pub fn node_mut(&mut self, path: &RelPath) -> Result<&mut FakeNode, SvnError> {
        self.nodes.get_mut(path).ok_or_else(|| not_found(path))
    }

    /// `path` and every node below it.
    pub fn subtree(&self, path: &RelPath) -> Vec<RelPath> {
        self.nodes
            .keys()
            .filter(|p| p.starts_with(path))
            .cloned()
            .collect()
    }

    /// Repository path of a working copy path.
    pub fn repo_path(&self, path: &RelPath) -> String {
        if path.is_root() {
            self.path_in_repo.clone()
        } else {
            format!("{}/{}", self.path_in_repo, path)
        }
    }

    /// Whether a path matches an ignore rule.
    fn ignored(&self, path: &RelPath) -> bool {
        let name = path.file_name().unwrap_or("");
        self.ignores.iter().any(|p| pattern_matches(p, name))
    }

    /// Info reported by `wc_info`.
    pub fn info(&self) -> WcInfo {
        let revs: Vec<u64> = self
            .nodes
            .values()
            .filter(|n| n.sched == Sched::Normal)
            .map(|n| n.rev)
            .collect();
        let base = self.nodes.get(&RelPath::root()).map_or(0, |n| n.rev);
        let (lo, hi) = (
            revs.iter().copied().min().unwrap_or(base),
            revs.iter().copied().max().unwrap_or(base),
        );
        WcInfo {
            root_path: self.root.to_string_lossy().into_owned(),
            repo_root_url: self.repo_root_url.clone(),
            repo_uuid: self.uuid.clone(),
            url: format!(
                "{}{}",
                self.repo_root_url.trim_end_matches('/'),
                self.path_in_repo
            ),
            base_rev: Rev(base),
            rev_range: (lo != hi).then_some((Rev(lo), Rev(hi))),
            format: self.format,
            store_pristine: self.store_pristine,
            path_in_repo: self.path_in_repo.clone(),
        }
    }

    /// Status of one node, `None` when it has nothing to show (unless `force`).
    fn status_of(
        &self,
        path: &RelPath,
        n: &FakeNode,
        q: StatusQuery,
        force: bool,
    ) -> Option<FileStatus> {
        let props_modified = n.props != n.base_props;
        let text = match n.sched {
            Sched::Unversioned if self.ignored(path) => NodeStatus::Ignored,
            Sched::Unversioned => NodeStatus::Unversioned,
            Sched::Added if !n.is_dir() && n.work.is_none() => NodeStatus::Missing,
            Sched::Added => NodeStatus::Added,
            Sched::Deleted => NodeStatus::Deleted,
            Sched::Normal if n.conflicted => NodeStatus::Conflicted,
            Sched::Normal if !n.is_dir() && n.work.is_none() => NodeStatus::Missing,
            Sched::Normal if !n.is_dir() && n.work != n.base => NodeStatus::Modified,
            Sched::Normal => NodeStatus::Normal,
        };
        match text {
            NodeStatus::Ignored if !q.include_ignored => return None,
            NodeStatus::Unversioned if !q.include_unversioned => return None,
            _ => {}
        }
        let interesting = text != NodeStatus::Normal
            || props_modified
            || n.tree_conflict
            || n.lock.is_some()
            || n.changelist.is_some()
            || n.moved_from.is_some()
            || n.moved_to.is_some();
        if path.is_root() || (!interesting && !force) {
            return None;
        }
        let content = n.work.as_deref().unwrap_or_default();
        Some(FileStatus {
            path: path.as_str().to_owned(),
            kind: n.kind,
            text,
            props: if props_modified {
                PropStatus::Modified
            } else if n.props.is_empty() {
                PropStatus::None
            } else {
                PropStatus::Normal
            },
            tree_conflict: n.tree_conflict,
            moved_from: n.moved_from.clone(),
            moved_to: n.moved_to.clone(),
            changelist: n.changelist.clone(),
            lock: n.lock.clone(),
            needs_lock: n.props.contains_key("svn:needs-lock"),
            remote: None,
            binary: content.iter().take(8000).any(|b| *b == 0),
            size: (!n.is_dir()).then_some(Size(content.len() as u64)),
        })
    }

    /// Status of a versioned node even when it is unremarkable (used to attach remote info).
    pub fn status_forced(&self, path: &RelPath) -> Option<FileStatus> {
        let q = StatusQuery {
            include_ignored: false,
            include_unversioned: false,
        };
        self.nodes
            .get(path)
            .and_then(|n| self.status_of(path, n, q, true))
    }

    /// Statuses of the whole working copy in path order.
    pub fn statuses(&self, q: StatusQuery) -> Vec<FileStatus> {
        self.nodes
            .iter()
            .filter_map(|(p, n)| self.status_of(p, n, q, false))
            .collect()
    }

    /// Selection expanded to every node below the selected paths (a directory means its content).
    pub fn expand_selection(&self, paths: &[String]) -> Result<Vec<RelPath>, SvnError> {
        let mut out = Vec::new();
        for p in paths {
            let rp = rel(p)?;
            self.node(&rp)?;
            for q in self.subtree(&rp) {
                if !out.contains(&q) {
                    out.push(q);
                }
            }
        }
        Ok(out)
    }
}
