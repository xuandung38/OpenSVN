//! In-memory repository state shared by the fake session and its commit editors.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::LocationSegment;
use bytes::Bytes;
use svn_types::{
    ChangeAction, ChangedPath, DirEntry, ErrorCode, LockInfo, NodeKind, PropChange, RelPath,
    RepoDisplayPath, Result, Revnum, SvnError, Timestamp,
};

/// Node content of one revision snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeData {
    /// Directory.
    Dir,
    /// File with content.
    File(Bytes),
}

/// One node in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Directory or file content.
    pub data: NodeData,
    /// Properties.
    pub props: BTreeMap<String, Bytes>,
    /// Revision in which this node (or its subtree for directories) last changed.
    pub created_rev: Revnum,
}

impl Node {
    /// Node kind.
    pub fn kind(&self) -> NodeKind {
        match self.data {
            NodeData::Dir => NodeKind::Dir,
            NodeData::File(_) => NodeKind::File,
        }
    }
}

/// Full tree of a revision; the root (`""`) always exists.
pub type Tree = BTreeMap<RelPath, Node>;

/// A committed revision.
#[derive(Debug, Clone)]
pub struct Revision {
    /// Snapshot after the revision.
    pub tree: Tree,
    /// Author.
    pub author: Option<String>,
    /// Log message.
    pub message: Option<String>,
    /// Commit time.
    pub date: Timestamp,
    /// Paths changed by the revision.
    pub changed: Vec<ChangedPath>,
}

/// Mutable repository state.
#[derive(Debug)]
pub struct RepoState {
    /// Revisions; index = revision number, `[0]` is the empty repository.
    pub revisions: Vec<Revision>,
    /// Locks by path.
    pub locks: HashMap<RelPath, LockInfo>,
    /// Open commit transactions.
    pub txns: HashSet<String>,
    /// Counter for transaction names.
    pub next_txn: u64,
    /// Fault injected into the next commit's `MERGE` (see `FakeRaSession::fail_next_merge`).
    pub merge_fault: Option<FakeMergeFault>,
    /// When set, every checked `DELETE` of a transaction fails with this server error and
    /// changes nothing (see `FakeRaSession::fail_txn_deletes`).
    pub txn_delete_fault: Option<String>,
}

/// A `MERGE` whose answer is lost, injected with `FakeRaSession::fail_next_merge`. Either way
/// `close_edit` fails with `CommitOutcomeUnknown` after `on_merge_sending`, without calling
/// `on_merged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeMergeFault {
    /// The server never applied the `MERGE`: the transaction is still there.
    LostBeforeMerge,
    /// The server created the revision, the answer was lost: the transaction is gone.
    LostAfterMerge,
}

fn root_node(rev: Revnum) -> Node {
    Node {
        data: NodeData::Dir,
        props: BTreeMap::new(),
        created_rev: rev,
    }
}

/// One change applied by `RepoState::commit_changes` (used to seed fake repositories).
#[derive(Debug, Clone)]
pub enum FakeChange {
    /// Create or replace a file (parents are created as needed).
    PutFile(RelPath, Bytes),
    /// Create a directory (parents are created as needed).
    PutDir(RelPath),
    /// Set or remove a property on a node.
    SetProp(RelPath, PropChange),
    /// Delete a node and its subtree.
    Delete(RelPath),
    /// Copy the node `from` of revision `rev` with its subtree to `to` (`svn copy`), recording
    /// the copy source in the changed paths, so the copy has history.
    Copy {
        /// Copy source.
        from: RelPath,
        /// Revision of the copy source.
        rev: Revnum,
        /// New path (its parent must exist).
        to: RelPath,
    },
}

impl RepoState {
    /// Empty repository at revision 0.
    pub fn new() -> Self {
        let mut tree = Tree::new();
        tree.insert(RelPath::root(), root_node(Revnum(0)));
        let r0 = Revision {
            tree,
            author: None,
            message: None,
            date: Timestamp(0),
            changed: Vec::new(),
        };
        Self {
            revisions: vec![r0],
            locks: HashMap::new(),
            txns: HashSet::new(),
            next_txn: 1,
            merge_fault: None,
            txn_delete_fault: None,
        }
    }

    /// Newest revision.
    pub fn youngest(&self) -> Revnum {
        Revnum((self.revisions.len() - 1) as u64)
    }

    /// Revision record, `PathNotFound` when it does not exist.
    pub fn revision(&self, rev: Option<Revnum>) -> Result<(Revnum, &Revision)> {
        let r = rev.unwrap_or_else(|| self.youngest());
        usize::try_from(r.0)
            .ok()
            .and_then(|i| self.revisions.get(i))
            .map(|rv| (r, rv))
            .ok_or_else(|| SvnError::new(ErrorCode::PathNotFound, format!("no such revision {r}")))
    }

    /// Location history of `path@peg`, youngest segment first, like svn_repos'
    /// `node_location_segments`: followed back through the copies recorded in the changed paths
    /// (`FakeChange::Copy` and copies made by commits), with a gap (`path: None`) between a copy
    /// source revision and the revision before the copy.
    pub fn location_segments(&self, path: &RelPath, peg: Revnum) -> Result<Vec<LocationSegment>> {
        let (_, at_peg) = self.revision(Some(peg))?;
        if !at_peg.tree.contains_key(path) {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("path '{path}' does not exist in revision {peg}"),
            ));
        }
        let segment = |start: u64, end: u64, path: Option<&RelPath>| LocationSegment {
            range_start: Revnum(start),
            range_end: Revnum(end),
            path: path.map(RepoDisplayPath::from_rel),
        };
        let mut out = Vec::new();
        let mut cur = path.clone();
        let mut end = peg.0;
        let mut n = peg.0;
        while n > 0 {
            let (_, rv) = self.revision(Some(Revnum(n)))?;
            // The deepest add of the node itself or of a directory above it.
            let born = rv
                .changed
                .iter()
                .filter(|c| matches!(c.action, ChangeAction::Added | ChangeAction::Replaced))
                .filter_map(|c| {
                    let p = RelPath::new(c.path.as_str().trim_start_matches('/')).ok()?;
                    cur.starts_with(&p).then_some((p, c))
                })
                .max_by_key(|(p, _)| p.as_str().len());
            let Some((at, c)) = born else {
                n -= 1;
                continue;
            };
            out.push(segment(n, end, Some(&cur)));
            let Some((src, src_rev)) = &c.copy_from else {
                return Ok(out);
            };
            let suffix = cur
                .as_str()
                .strip_prefix(at.as_str())
                .unwrap_or("")
                .trim_start_matches('/')
                .to_owned();
            let src = RelPath::new(src.as_str().trim_start_matches('/'))?;
            cur = if suffix.is_empty() {
                src
            } else if src.is_root() {
                RelPath::new(&suffix)?
            } else {
                RelPath::new(&format!("{src}/{suffix}"))?
            };
            if src_rev.0 + 1 < n {
                out.push(segment(src_rev.0 + 1, n - 1, None));
            }
            end = src_rev.0;
            n = src_rev.0;
        }
        out.push(segment(0, end, Some(&cur)));
        Ok(out)
    }

    /// Directory entry for `path` in `tree`.
    pub fn entry(tree: &Tree, path: &RelPath, lock: Option<&LockInfo>) -> Option<DirEntry> {
        let node = tree.get(path)?;
        Some(DirEntry {
            name: path.file_name().unwrap_or("").to_owned(),
            kind: node.kind(),
            size: match &node.data {
                NodeData::File(b) => Some(b.len() as u64),
                NodeData::Dir => None,
            },
            created_rev: Some(node.created_rev),
            last_author: None,
            time: None,
            has_props: !node.props.is_empty(),
            lock: lock.cloned(),
        })
    }

    /// Applies `changes` as a new revision and returns it.
    pub fn commit_changes(
        &mut self,
        author: &str,
        message: &str,
        changes: &[FakeChange],
    ) -> Result<Revnum> {
        let rev = Revnum(self.revisions.len() as u64);
        let mut tree = self
            .revisions
            .last()
            .map(|r| r.tree.clone())
            .unwrap_or_default();
        let mut changed = Vec::new();
        for c in changes {
            if let FakeChange::Copy { from, rev: src, to } = c {
                let (_, source) = self.revision(Some(*src))?;
                copy_node(&mut tree, &source.tree, from, *src, to, rev, &mut changed)?;
                continue;
            }
            apply_change(&mut tree, rev, c, &mut changed)?;
        }
        if let Some(root) = tree.get_mut(&RelPath::root()) {
            root.created_rev = rev;
        }
        self.install(
            rev,
            tree,
            Some(author.to_owned()),
            Some(message.to_owned()),
            changed,
        );
        Ok(rev)
    }

    /// Stores a finished tree as the next revision.
    pub fn install(
        &mut self,
        rev: Revnum,
        tree: Tree,
        author: Option<String>,
        message: Option<String>,
        changed: Vec<ChangedPath>,
    ) {
        let date = Timestamp(1_760_000_000_000_000 + (rev.0 as i64) * 1_000_000);
        self.revisions.push(Revision {
            tree,
            author,
            message,
            date,
            changed,
        });
    }
}

fn ensure_parents(tree: &mut Tree, path: &RelPath, rev: Revnum, changed: &mut Vec<ChangedPath>) {
    let mut parent = path.parent();
    while let Some(p) = parent {
        if !tree.contains_key(&p) {
            tree.insert(p.clone(), root_node(rev));
            changed.push(changed_path(&p, ChangeAction::Added, NodeKind::Dir));
        }
        parent = p.parent();
    }
}

fn changed_path(path: &RelPath, action: ChangeAction, kind: NodeKind) -> ChangedPath {
    ChangedPath {
        path: RepoDisplayPath::from_rel(path),
        action,
        kind,
        copy_from: None,
        text_mods: kind == NodeKind::File,
        prop_mods: false,
    }
}

/// Copies `from` of `source` (revision `src`) with its subtree to `to` in `tree` (revision `rev`
/// being built), like a server-side `svn copy`: the copy root records its source.
fn copy_node(
    tree: &mut Tree,
    source: &Tree,
    from: &RelPath,
    src: Revnum,
    to: &RelPath,
    rev: Revnum,
    changed: &mut Vec<ChangedPath>,
) -> Result<()> {
    let kind = source.get(from).map(Node::kind).ok_or_else(|| {
        SvnError::new(
            ErrorCode::PathNotFound,
            format!("copy source {from}@{src} does not exist"),
        )
    })?;
    if tree.contains_key(to) {
        return Err(SvnError::new(
            ErrorCode::OutOfDate,
            format!("copy target {to} already exists"),
        ));
    }
    if to.parent().is_some_and(|p| !tree.contains_key(&p)) {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("parent of copy target {to} does not exist"),
        ));
    }
    for (k, n) in source.iter().filter(|(k, _)| k.starts_with(from)) {
        let suffix = k
            .as_str()
            .strip_prefix(from.as_str())
            .unwrap_or("")
            .trim_start_matches('/');
        let mut n = n.clone();
        let target = if suffix.is_empty() {
            n.created_rev = rev;
            to.clone()
        } else if to.is_root() {
            RelPath::new(suffix)?
        } else {
            RelPath::new(&format!("{to}/{suffix}"))?
        };
        tree.insert(target, n);
    }
    changed.push(ChangedPath {
        path: RepoDisplayPath::from_rel(to),
        action: ChangeAction::Added,
        kind,
        copy_from: Some((RepoDisplayPath::from_rel(from), src)),
        text_mods: false,
        prop_mods: false,
    });
    let mut parent = to.parent();
    while let Some(p) = parent {
        if let Some(n) = tree.get_mut(&p) {
            n.created_rev = rev;
        }
        parent = p.parent();
    }
    Ok(())
}

/// Applies one change to a tree, recording it. Shared by seeding and the commit editor.
pub fn apply_change(
    tree: &mut Tree,
    rev: Revnum,
    change: &FakeChange,
    changed: &mut Vec<ChangedPath>,
) -> Result<()> {
    match change {
        FakeChange::PutFile(path, content) => {
            ensure_parents(tree, path, rev, changed);
            let existed = tree.contains_key(path);
            let props = tree.get(path).map(|n| n.props.clone()).unwrap_or_default();
            tree.insert(
                path.clone(),
                Node {
                    data: NodeData::File(content.clone()),
                    props,
                    created_rev: rev,
                },
            );
            let action = if existed {
                ChangeAction::Modified
            } else {
                ChangeAction::Added
            };
            changed.push(changed_path(path, action, NodeKind::File));
        }
        FakeChange::PutDir(path) => {
            if !tree.contains_key(path) {
                ensure_parents(tree, path, rev, changed);
                tree.insert(path.clone(), root_node(rev));
                changed.push(changed_path(path, ChangeAction::Added, NodeKind::Dir));
            }
        }
        FakeChange::SetProp(path, p) => {
            let node = tree.get_mut(path).ok_or_else(|| {
                SvnError::new(ErrorCode::PathNotFound, format!("no such node {path}"))
            })?;
            match &p.value {
                Some(v) => node.props.insert(p.name.clone(), v.clone()),
                None => node.props.remove(&p.name),
            };
            node.created_rev = rev;
            let kind = node.kind();
            let mut cp = changed_path(path, ChangeAction::Modified, kind);
            cp.text_mods = false;
            cp.prop_mods = true;
            changed.push(cp);
        }
        FakeChange::Delete(path) => {
            let kind = tree.get(path).map(Node::kind).ok_or_else(|| {
                SvnError::new(ErrorCode::PathNotFound, format!("no such node {path}"))
            })?;
            tree.retain(|k, _| !k.starts_with(path));
            changed.push(changed_path(path, ChangeAction::Deleted, kind));
        }
        FakeChange::Copy { .. } => {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "a copy needs the repository history; seed it through commit_changes",
            ));
        }
    }
    // Ancestors changed too.
    let mut parent = match change {
        FakeChange::Delete(p)
        | FakeChange::PutFile(p, _)
        | FakeChange::PutDir(p)
        | FakeChange::SetProp(p, _) => p.parent(),
        FakeChange::Copy { to, .. } => to.parent(),
    };
    while let Some(p) = parent {
        if let Some(n) = tree.get_mut(&p) {
            n.created_rev = rev;
        }
        parent = p.parent();
    }
    Ok(())
}

/// Immediate children of `dir` in `tree`.
pub fn children<'a>(
    tree: &'a Tree,
    dir: &'a RelPath,
) -> impl Iterator<Item = (&'a RelPath, &'a Node)> {
    tree.iter()
        .filter(move |(k, _)| !k.is_root() && k.parent().as_ref() == Some(dir))
}

impl Default for RepoState {
    fn default() -> Self {
        Self::new()
    }
}
