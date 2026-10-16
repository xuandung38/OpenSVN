//! In-memory repository state shared by the fake session and its commit editors.

use std::collections::{BTreeMap, HashMap, HashSet};

use bytes::Bytes;
use svn_types::{
    ChangeAction, ChangedPath, DirEntry, ErrorCode, LockInfo, NodeKind, PropChange, RelPath,
    RepoDisplayPath, Result, Revnum, SvnError, Timestamp,
};

use crate::session::LocationSegment;

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
    /// Checked `DELETE`s left before `txn_delete_fault` clears itself; `None` keeps it until
    /// it is reset (see `FakeRaSession::fail_txn_deletes_times`).
    pub txn_delete_faults_left: Option<u32>,
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
    /// Copy the node `from`@`rev` with its subtree to `to` (which must not exist; parents are
    /// created as needed), keeping its history like `svn copy`. Only
    /// [`RepoState::commit_changes`] applies it: the copy source lives in an older revision.
    Copy {
        /// Source path.
        from: RelPath,
        /// Source revision.
        rev: Revnum,
        /// Destination path.
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
            txn_delete_faults_left: None,
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
            match c {
                FakeChange::Copy {
                    from,
                    rev: from_rev,
                    to,
                } => {
                    let (_, src) = self.revision(Some(*from_rev))?;
                    copy_subtree(&mut tree, &src.tree, from, *from_rev, to, rev, &mut changed)?;
                }
                _ => apply_change(&mut tree, rev, c, &mut changed)?,
            }
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

    /// History of the node at `path`@`peg` as location segments, youngest first, cropped to
    /// `end..=start` (see `RaSession::get_location_segments`).
    ///
    /// A node is born in the revision whose changed paths add or replace it or one of its
    /// parents (the deepest such path wins); a birth by copy continues the history at the copy
    /// source, with a gap for the revisions in between, like `svn_repos_node_location_segments`.
    pub fn location_segments(
        &self,
        path: &RelPath,
        peg: Option<Revnum>,
        start: Option<Revnum>,
        end: Option<Revnum>,
    ) -> Result<Vec<LocationSegment>> {
        let (peg, rv) = self.revision(peg)?;
        let start = start.unwrap_or(peg);
        let end = end.unwrap_or(Revnum(0));
        if start > peg || end > start {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("invalid revision range {}:{} for peg {peg}", start.0, end.0),
            ));
        }
        if !rv.tree.contains_key(path) {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("path '{path}' not found in revision {peg}"),
            ));
        }
        // Uncropped history, youngest first.
        let mut history: Vec<LocationSegment> = Vec::new();
        let (mut cur, mut top) = (path.clone(), peg);
        loop {
            let birth = (1..=top.0).rev().find_map(|r| {
                let rv = usize::try_from(r)
                    .ok()
                    .and_then(|i| self.revisions.get(i))?;
                rv.changed
                    .iter()
                    .filter(|c| matches!(c.action, ChangeAction::Added | ChangeAction::Replaced))
                    .filter_map(|c| {
                        let at = RelPath::new(c.path.as_str().trim_start_matches('/')).ok()?;
                        cur.starts_with(&at).then_some((at, c))
                    })
                    .max_by_key(|(at, _)| at.as_str().len())
                    .map(|(at, c)| (Revnum(r), at, c.copy_from.clone()))
            });
            let Some((born, at, copy_from)) = birth else {
                history.push(LocationSegment {
                    range_start: Revnum(0),
                    range_end: top,
                    path: Some(cur),
                });
                break;
            };
            history.push(LocationSegment {
                range_start: born,
                range_end: top,
                path: Some(cur.clone()),
            });
            let Some((src, src_rev)) = copy_from else {
                break;
            };
            if src_rev.0 + 1 < born.0 {
                history.push(LocationSegment {
                    range_start: Revnum(src_rev.0 + 1),
                    range_end: Revnum(born.0 - 1),
                    path: None,
                });
            }
            let suffix = cur
                .as_str()
                .strip_prefix(at.as_str())
                .unwrap_or("")
                .trim_start_matches('/');
            let src = src.as_str().trim_start_matches('/');
            cur = RelPath::new(
                &[src, suffix]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("/"),
            )?;
            top = src_rev;
        }
        Ok(history
            .into_iter()
            .filter(|s| s.range_start <= start && s.range_end >= end)
            .map(|mut s| {
                s.range_start = s.range_start.max(end);
                s.range_end = s.range_end.min(start);
                s
            })
            .collect())
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

/// Adds `to` as a copy of `from`@`from_rev` (taken from `src`) with its subtree. The copy root
/// is new in `rev`; copied children keep their last-changed revisions, as in FSFS.
fn copy_subtree(
    tree: &mut Tree,
    src: &Tree,
    from: &RelPath,
    from_rev: Revnum,
    to: &RelPath,
    rev: Revnum,
    changed: &mut Vec<ChangedPath>,
) -> Result<()> {
    let kind = src.get(from).map(Node::kind).ok_or_else(|| {
        SvnError::new(
            ErrorCode::PathNotFound,
            format!("copy source {from}@{from_rev} not found"),
        )
    })?;
    if tree.contains_key(to) {
        return Err(SvnError::new(
            ErrorCode::OutOfDate,
            format!("copy target {to} already exists"),
        ));
    }
    ensure_parents(tree, to, rev, changed);
    for (k, n) in src.iter().filter(|(k, _)| k.starts_with(from)) {
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
        copy_from: Some((RepoDisplayPath::from_rel(from), from_rev)),
        text_mods: false,
        ..changed_path(to, ChangeAction::Added, kind)
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
        FakeChange::Copy { from, to, .. } => {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("the copy of {from} to {to} needs the repository's history"),
            ));
        }
    }
    // Ancestors changed too.
    let mut parent = match change {
        FakeChange::Delete(p)
        | FakeChange::PutFile(p, _)
        | FakeChange::PutDir(p)
        | FakeChange::SetProp(p, _)
        | FakeChange::Copy { to: p, .. } => p.parent(),
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
