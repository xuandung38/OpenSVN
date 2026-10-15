//! Engine `WcStatus` -> IPC `FileStatus` (doc 03 section 2).

use svn_types::{NodeKind, NodeStatus as SvnNodeStatus, PropStatus as SvnPropStatus, WcStatus};

use crate::ids::{Rev, Size};
use crate::models::{FileKind, FileStatus, LockInfo, NodeStatus, PropStatus, RemoteStatus};
use crate::time_fmt::timestamp_to_iso;

/// IPC view of a lock. The token never crosses the IPC boundary: whether the lock is this
/// working copy's own is reported as `mine`, and the token stays in the engine.
fn map_lock(lock: &svn_types::LockInfo, mine: bool) -> LockInfo {
    LockInfo {
        owner: lock.owner.clone(),
        comment: lock.comment.clone(),
        created_at: timestamp_to_iso(lock.created),
        token: None,
        mine,
    }
}

/// Maps an engine node status. `None` has no IPC equivalent and is shown as unversioned.
/// Returns `None` for `Excluded` nodes, which the UI never lists.
fn map_node_status(s: SvnNodeStatus) -> Option<NodeStatus> {
    Some(match s {
        SvnNodeStatus::None | SvnNodeStatus::Unversioned => NodeStatus::Unversioned,
        SvnNodeStatus::Normal => NodeStatus::Normal,
        SvnNodeStatus::Added => NodeStatus::Added,
        SvnNodeStatus::Deleted => NodeStatus::Deleted,
        SvnNodeStatus::Modified => NodeStatus::Modified,
        SvnNodeStatus::Replaced => NodeStatus::Replaced,
        SvnNodeStatus::Missing => NodeStatus::Missing,
        SvnNodeStatus::Obstructed => NodeStatus::Obstructed,
        SvnNodeStatus::Incomplete => NodeStatus::Incomplete,
        SvnNodeStatus::Ignored => NodeStatus::Ignored,
        SvnNodeStatus::Conflicted => NodeStatus::Conflicted,
        SvnNodeStatus::External => NodeStatus::External,
        SvnNodeStatus::Excluded => return None,
    })
}

fn map_prop_status(s: SvnPropStatus) -> PropStatus {
    match s {
        SvnPropStatus::None => PropStatus::None,
        SvnPropStatus::Normal => PropStatus::Normal,
        SvnPropStatus::Modified => PropStatus::Modified,
        SvnPropStatus::Conflicted => PropStatus::Conflicted,
    }
}

fn map_kind(k: NodeKind) -> FileKind {
    match k {
        NodeKind::Dir => FileKind::Dir,
        NodeKind::Symlink => FileKind::Symlink,
        NodeKind::File | NodeKind::None | NodeKind::Unknown => FileKind::File,
    }
}

/// Converts an engine status to the IPC shape; `None` when the node must not be listed.
pub fn file_status_from_wc(s: &WcStatus) -> Option<FileStatus> {
    let text = map_node_status(s.text)?;
    let local_token = s.lock.as_ref().map(|l| l.token.as_str());
    Some(FileStatus {
        path: s.path.as_str().to_owned(),
        kind: map_kind(s.kind),
        text,
        props: map_prop_status(s.props),
        tree_conflict: s.tree_conflict,
        moved_from: s.moved_from.as_ref().map(|p| p.as_str().to_owned()),
        moved_to: s.moved_to.as_ref().map(|p| p.as_str().to_owned()),
        changelist: s.changelist.clone(),
        lock: s.lock.as_ref().map(|l| map_lock(l, s.lock_is_mine)),
        needs_lock: s.needs_lock,
        remote: s.remote.as_ref().map(|r| RemoteStatus {
            out_of_date: r.out_of_date,
            remote_rev: r.remote_rev.map(|rev| Rev(rev.0)),
            remote_lock: r
                .remote_lock
                .as_ref()
                .map(|l| map_lock(l, local_token == Some(l.token.as_str()))),
        }),
        binary: s.binary,
        size: s.size.map(Size),
    })
}
