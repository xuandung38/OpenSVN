//! Conversions between the working copy layer (`svn-client`, `svn-wc`) and the IPC model.
//!
//! * Statuses: `WcStatus` -> `FileStatus` through the shared IPC mapping, which never fills a
//!   lock token: whether a lock is this working copy's own is reported as `lock.mine`; the token
//!   itself stays in `wc.db` and never crosses the IPC boundary.
//! * Property lists: two stored property lists -> the `PropChange` rows of a diff.
//! * Conflict resolution: the IPC choice -> the `svn-client` choice and scope.
//!
//! Errors need no mapping here: engine calls return `SvnError`, which the command boundary turns
//! into an `AppError` once (`AppError::from(&SvnError)`).

use std::collections::BTreeMap;

use opensvn_ipc::{FileStatus, NodeStatus, PropChange};
use svn_types::WcStatus;
use svn_wc::db::Props;

/// IPC status of one node; `None` for nodes the UI never lists (excluded).
pub(super) fn file_status(s: &WcStatus) -> Option<FileStatus> {
    let mut out = opensvn_ipc::file_status_from_wc(s)?;
    // A node that only exists in the repository (remote status of a path added there) has no
    // local state: it is shown as an unremarkable node with its remote side, not as unversioned.
    if s.text == svn_types::NodeStatus::None && s.remote.is_some() {
        out.text = NodeStatus::Normal;
    }
    Some(out)
}

/// Whether a property is bookkeeping that the server or the working copy adds by itself and a
/// diff never shows.
fn is_internal_prop(name: &str) -> bool {
    name.starts_with("svn:entry:") || name.starts_with("svn:wc:")
}

/// Property differences between `before` and `after`, sorted by name. Values are shown as text
/// (invalid UTF-8 is replaced).
pub(super) fn prop_changes(before: &Props, after: &Props) -> Vec<PropChange> {
    let text = |v: &Vec<u8>| String::from_utf8_lossy(v).into_owned();
    let index = |p: &Props| -> BTreeMap<String, Vec<u8>> {
        p.iter()
            .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), v.clone()))
            .filter(|(k, _)| !is_internal_prop(k))
            .collect()
    };
    let (old, new) = (index(before), index(after));
    let mut names: Vec<&String> = old.keys().chain(new.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|n| old.get(*n) != new.get(*n))
        .map(|n| PropChange {
            name: n.clone(),
            before: old.get(n).map(text),
            after: new.get(n).map(text),
        })
        .collect()
}

/// Property list of a file read from the repository.
pub(super) fn props_from_changes(props: &[svn_types::PropChange]) -> Props {
    props
        .iter()
        .filter_map(|p| {
            p.value
                .as_ref()
                .map(|v| (p.name.as_bytes().to_vec(), v.to_vec()))
        })
        .collect()
}

/// `svn-client` choice and scope for an IPC resolve request: "mine" and "theirs" take the whole
/// file of that side (`mine-full`, `theirs-full`), and every kind of conflict on the path is
/// resolved.
#[cfg_attr(not(test), allow(dead_code))] // used by the local operations (resolve)
pub(super) fn resolve_choice(
    choice: opensvn_ipc::ResolveChoice,
) -> (
    svn_client::local::ResolveChoice,
    svn_client::local::ResolveScope,
) {
    use opensvn_ipc::ResolveChoice as Ipc;
    use svn_client::local::ResolveChoice as Client;
    let c = match choice {
        Ipc::Mine => Client::MineFull,
        Ipc::Theirs => Client::TheirsFull,
        Ipc::Working => Client::Working,
        Ipc::Base => Client::Base,
        Ipc::MineConflict => Client::MineConflict,
    };
    (c, svn_client::local::ResolveScope::ALL)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use svn_types::{
        Depth, LockInfo, NodeKind, PropStatus, RelPath, RemoteStatus, RepoDisplayPath, Revnum,
        Timestamp,
    };

    use super::*;

    fn lock(token: &str) -> LockInfo {
        LockInfo {
            path: RepoDisplayPath::parse("/a.txt").unwrap(),
            token: token.to_owned(),
            owner: "alice".to_owned(),
            comment: Some("editing".to_owned()),
            created: Timestamp(0),
            expires: None,
        }
    }

    fn status(text: svn_types::NodeStatus) -> WcStatus {
        WcStatus {
            path: RelPath::new("a.txt").unwrap(),
            kind: NodeKind::File,
            text,
            props: PropStatus::None,
            tree_conflict: false,
            moved_from: None,
            moved_to: None,
            changelist: None,
            lock: None,
            lock_is_mine: false,
            needs_lock: false,
            binary: false,
            size: Some(3),
            remote: None,
            switched: false,
            file_external: false,
            depth: Depth::Unknown,
            base_rev: Some(Revnum(1)),
        }
    }

    #[test]
    fn lock_tokens_never_reach_the_ui() {
        let mut s = status(svn_types::NodeStatus::Normal);
        s.lock = Some(lock("opaquelocktoken:mine"));
        s.lock_is_mine = true;
        s.remote = Some(RemoteStatus {
            out_of_date: false,
            remote_rev: None,
            remote_lock: Some(lock("opaquelocktoken:mine")),
        });
        let f = file_status(&s).unwrap();
        let local = f.lock.unwrap();
        assert!(local.mine);
        assert_eq!(local.token, None);
        let remote = f.remote.unwrap().remote_lock.unwrap();
        assert!(remote.mine, "same token as the working copy lock");
        assert_eq!(remote.token, None);
    }

    #[test]
    fn a_node_only_in_the_repository_is_not_unversioned() {
        let mut s = status(svn_types::NodeStatus::None);
        assert_eq!(file_status(&s).unwrap().text, NodeStatus::Unversioned);
        s.remote = Some(RemoteStatus {
            out_of_date: true,
            remote_rev: Some(Revnum(2)),
            remote_lock: None,
        });
        assert_eq!(file_status(&s).unwrap().text, NodeStatus::Normal);
        assert!(file_status(&status(svn_types::NodeStatus::Excluded)).is_none());
    }

    fn props(pairs: &[(&str, &str)]) -> Props {
        pairs
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn prop_changes_list_added_changed_and_deleted_names_in_order() {
        let before = props(&[("b", "1"), ("a", "same"), ("c", "gone")]);
        let after = props(&[
            ("a", "same"),
            ("b", "2"),
            ("d", "new"),
            ("svn:entry:uuid", "x"),
        ]);
        let got = prop_changes(&before, &after);
        let rows: Vec<(&str, Option<&str>, Option<&str>)> = got
            .iter()
            .map(|c| (c.name.as_str(), c.before.as_deref(), c.after.as_deref()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("b", Some("1"), Some("2")),
                ("c", Some("gone"), None),
                ("d", None, Some("new")),
            ]
        );
    }

    #[test]
    fn resolve_choices_take_whole_sides_and_all_conflict_kinds() {
        use opensvn_ipc::ResolveChoice as Ipc;
        use svn_client::local::ResolveChoice as Client;
        let cases = [
            (Ipc::Mine, Client::MineFull),
            (Ipc::Theirs, Client::TheirsFull),
            (Ipc::Working, Client::Working),
            (Ipc::Base, Client::Base),
        ];
        for (ipc, want) in cases {
            let (got, scope) = resolve_choice(ipc);
            assert_eq!(got, want);
            assert!(scope.text && scope.props && scope.tree);
        }
    }
}
