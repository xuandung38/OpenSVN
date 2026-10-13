//! Building and storing conflict skels for update and switch.

use rusqlite::Transaction;
use svn_types::{NodeKind, RelPath, Result};
use svn_wc::conflicts::{
    Conflict, ConflictSkel, Location, Operation, OperationInfo, Origin, PropConflict, TextConflict,
    TreeAction, TreeConflict, TreeReason,
};

use crate::wc::txn;

/// One side of the operation origin.
#[derive(Debug, Clone)]
pub struct Side {
    /// Repository path (no leading slash).
    pub repos_relpath: String,
    /// Peg revision.
    pub rev: u64,
    /// Node kind.
    pub kind: NodeKind,
}

fn kind_word(k: NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Dir => "dir",
        NodeKind::Symlink => "symlink",
        NodeKind::None => "none",
        NodeKind::Unknown => "unknown",
    }
}

fn origin(root: &str, uuid: &str, side: Option<&Side>) -> Origin {
    match side {
        None => Origin::Unset,
        Some(s) => Origin::Subversion(Location {
            repos_root_url: root.to_owned(),
            repos_uuid: Some(uuid.to_owned()),
            repos_relpath: s.repos_relpath.clone(),
            peg_rev: i64::try_from(s.rev).unwrap_or(i64::MAX),
            node_kind: kind_word(s.kind).to_owned(),
        }),
    }
}

/// `(operation (left right))` for an update or switch.
pub fn operation_info(
    switch: bool,
    root: &str,
    uuid: &str,
    old: Option<&Side>,
    new: Option<&Side>,
) -> OperationInfo {
    OperationInfo {
        operation: if switch {
            Operation::Switch
        } else {
            Operation::Update
        },
        left: origin(root, uuid, old),
        right: origin(root, uuid, new),
    }
}

/// A conflict to add to a node.
pub enum NewConflict {
    /// Text conflict with its marker files.
    Text(TextConflict),
    /// Property conflict.
    Prop(PropConflict),
    /// Tree conflict.
    Tree(TreeReason, TreeAction),
}

impl NewConflict {
    fn into_conflict(self) -> Conflict {
        match self {
            Self::Text(t) => Conflict::Text(t),
            Self::Prop(p) => Conflict::Prop(p),
            Self::Tree(reason, action) => Conflict::Tree(TreeConflict {
                reason,
                action,
                move_src_op_root: None,
                move_dst_op_root: None,
            }),
        }
    }
}

/// Adds `conflicts` to the conflict record of `path` inside the caller's transaction. A conflict
/// of a kind that is already recorded replaces the old one; the operation info is refreshed.
pub fn record(
    tx: &Transaction<'_>,
    wc_id: i64,
    path: &RelPath,
    info: &OperationInfo,
    conflicts: Vec<NewConflict>,
) -> Result<()> {
    let mut skel = match txn::read_conflict_skel(tx, wc_id, path)? {
        Some(s) => ConflictSkel::from_skel(&s)?,
        None => ConflictSkel::default(),
    };
    skel.operation = Some(info.clone());
    for c in conflicts {
        let c = c.into_conflict();
        skel.conflicts
            .retain(|e| std::mem::discriminant(e) != std::mem::discriminant(&c));
        skel.add(c)?;
    }
    txn::write_conflict_skel(tx, wc_id, path, &skel.to_skel())
}
