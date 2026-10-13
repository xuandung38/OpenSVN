//! Conversions from engine types to the IPC model, shared by the fake and the real engine.

use opensvn_ipc::time_fmt::timestamp_to_iso;
use opensvn_ipc::{ChangeAction, ChangedPath, CopyFrom, EntryKind, LogEntry, PathKind, Rev};
use svn_types::{NodeKind, Revnum};

/// IPC revision from an engine revision.
pub(crate) fn rev(r: Revnum) -> Rev {
    Rev(r.0)
}

/// Engine revision from an IPC revision.
pub(crate) fn revnum(r: Option<Rev>) -> Option<Revnum> {
    r.map(|r| Revnum(r.0))
}

/// Entry kind of a listed node (anything that is not a directory is shown as a file).
pub(crate) fn entry_kind(kind: NodeKind) -> EntryKind {
    if kind == NodeKind::Dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

/// IPC lock description of a repository lock seen in a listing: whether it is this working
/// copy's own lock is only known from the working copy, so the token is not exposed here.
pub(crate) fn lock(l: svn_types::LockInfo) -> opensvn_ipc::LockInfo {
    opensvn_ipc::LockInfo {
        owner: l.owner,
        comment: l.comment,
        created_at: timestamp_to_iso(l.created),
        token: None,
        mine: false,
    }
}

/// IPC log entry.
pub(crate) fn log_entry(e: svn_types::LogEntry) -> LogEntry {
    let paths: Vec<ChangedPath> = e
        .changed_paths
        .into_iter()
        .map(|c| ChangedPath {
            path: c.path.to_string(),
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
            copy_from: c.copy_from.map(|(path, r)| CopyFrom {
                path: path.to_string(),
                rev: rev(r),
            }),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use svn_types::{RepoDisplayPath, Revnum};

    use super::*;

    #[test]
    fn log_entries_carry_display_paths_as_text() {
        let entry = svn_types::LogEntry {
            rev: Revnum(7),
            author: Some("alice".into()),
            date: None,
            message: None,
            changed_paths: vec![svn_types::ChangedPath {
                path: RepoDisplayPath::parse("/branches/a b/x.txt").unwrap(),
                action: svn_types::ChangeAction::Added,
                kind: NodeKind::File,
                copy_from: Some((RepoDisplayPath::parse("/trunk/x.txt").unwrap(), Revnum(5))),
                text_mods: true,
                prop_mods: false,
            }],
            has_children: false,
        };
        let mapped = log_entry(entry);
        assert_eq!(mapped.rev, Rev(7));
        assert_eq!(mapped.message, "");
        assert_eq!(mapped.date, "");
        let paths = mapped.changed_paths.unwrap();
        assert_eq!(paths[0].path, "/branches/a b/x.txt");
        assert_eq!(paths[0].action, ChangeAction::A);
        assert_eq!(paths[0].kind, PathKind::File);
        let from = paths[0].copy_from.as_ref().unwrap();
        assert_eq!((from.path.as_str(), from.rev), ("/trunk/x.txt", Rev(5)));
    }
}
