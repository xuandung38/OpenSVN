//! Building and storing conflict skels for update and switch.

use svn_types::{NodeKind, RelPath, Result};
use svn_wc::conflicts::{
    Conflict, Location, Operation, OperationInfo, Origin, PropConflict, TextConflict, TreeAction,
    TreeConflict, TreeReason,
};
use svn_wc::db::WcTxn;

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
    /// Tree conflict "local moved away" on the source op root `src` of a move to `dst`.
    MovedAway {
        /// Incoming action.
        action: TreeAction,
        /// Source op root of the move (the conflict victim).
        src: String,
        /// Destination op root of the move.
        dst: String,
    },
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
            Self::MovedAway { action, src, dst } => Conflict::Tree(TreeConflict {
                reason: TreeReason::MovedAway,
                action,
                move_src_op_root: Some(src),
                move_dst_op_root: Some(dst),
            }),
        }
    }
}

/// Adds `conflicts` to the conflict record of `path` inside the caller's transaction. A conflict
/// of a kind that is already recorded replaces the old one; the operation info is refreshed.
pub fn record(
    t: &WcTxn<'_>,
    path: &RelPath,
    info: &OperationInfo,
    conflicts: Vec<NewConflict>,
) -> Result<()> {
    t.add_conflicts(
        path,
        info,
        conflicts.into_iter().map(NewConflict::into_conflict),
    )
}
