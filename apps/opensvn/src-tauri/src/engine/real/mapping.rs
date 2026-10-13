//! Conversions from engine types to the IPC model.

use opensvn_ipc::time_fmt::timestamp_to_iso;
use opensvn_ipc::{ChangeAction, ChangedPath, CopyFrom, EntryKind, LogEntry, PathKind, Rev};
use svn_types::{NodeKind, Revnum};

/// IPC revision from an engine revision.
pub(super) fn rev(r: Revnum) -> Rev {
    Rev(r.0)
}

/// Engine revision from an IPC revision.
pub(super) fn revnum(r: Option<Rev>) -> Option<Revnum> {
    r.map(|r| Revnum(r.0))
}

/// Entry kind of a listed node (anything that is not a directory is shown as a file).
pub(super) fn entry_kind(kind: NodeKind) -> EntryKind {
    if kind == NodeKind::Dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

/// IPC lock description of a repository lock seen in a listing: whether it is this working
/// copy's own lock is only known from the working copy, so the token is not exposed here.
pub(super) fn lock(l: svn_types::LockInfo) -> opensvn_ipc::LockInfo {
    opensvn_ipc::LockInfo {
        owner: l.owner,
        comment: l.comment,
        created_at: timestamp_to_iso(l.created),
        token: None,
        mine: false,
    }
}

/// IPC log entry.
pub(super) fn log_entry(e: svn_types::LogEntry) -> LogEntry {
    let paths: Vec<ChangedPath> = e
        .changed_paths
        .into_iter()
        .map(|c| ChangedPath {
            path: c.path,
            action: match c.action {
                svn_types::ChangeAction::Added => ChangeAction::A,
                svn_types::ChangeAction::Modified => ChangeAction::M,
                svn_types::ChangeAction::Deleted => ChangeAction::D,
                svn_types::ChangeAction::Replaced => ChangeAction::R,
            },
            kind: match c.kind {
                NodeKind::File => PathKind::File,
                NodeKind::Dir => PathKind::Dir,
                _ => PathKind::Unknown,
            },
            copy_from: c.copy_from.map(|(path, r)| CopyFrom { path, rev: rev(r) }),
            text_mods: Some(c.text_mods),
            prop_mods: Some(c.prop_mods),
        })
        .collect();
    LogEntry {
        rev: rev(e.rev),
        author: e.author,
        date: e.date.map_or_else(String::new, timestamp_to_iso),
        message: e.message.unwrap_or_default(),
        changed_paths: (!paths.is_empty()).then_some(paths),
        has_children: Some(e.has_children),
    }
}
