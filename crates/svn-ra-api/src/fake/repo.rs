//! In-memory repository state shared by the fake session and its commit editors.

use std::collections::{BTreeMap, HashMap, HashSet};

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
    }
    // Ancestors changed too.
    let mut parent = match change {
        FakeChange::Delete(p)
        | FakeChange::PutFile(p, _)
        | FakeChange::PutDir(p)
        | FakeChange::SetProp(p, _) => p.parent(),
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
