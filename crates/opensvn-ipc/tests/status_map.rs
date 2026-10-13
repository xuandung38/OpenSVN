#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opensvn_ipc::{FileKind, NodeStatus, PropStatus, file_status_from_wc};
use svn_types::{
    Depth, LockInfo, NodeKind, NodeStatus as S, PropStatus as P, RelPath, RemoteStatus,
    RepoDisplayPath, Revnum, Timestamp, WcStatus,
};

fn base() -> WcStatus {
    WcStatus {
        path: RelPath::new("src/a.txt").expect("valid"),
        kind: NodeKind::File,
        text: S::Modified,
        props: P::Normal,
        tree_conflict: false,
        moved_from: None,
        moved_to: None,
        changelist: Some("cl".into()),
        lock: None,
        lock_is_mine: false,
        needs_lock: true,
        binary: false,
        size: Some(42),
        remote: None,
        switched: false,
        depth: Depth::Infinity,
        base_rev: Some(Revnum(5)),
    }
}

fn lock(token: &str) -> LockInfo {
    LockInfo {
        path: RepoDisplayPath::parse("/a").unwrap(),
        token: token.into(),
        owner: "alice".into(),
        comment: None,
        created: Timestamp(1_000_000),
        expires: None,
    }
}

#[test]
fn maps_basic_fields() {
    let f = file_status_from_wc(&base()).expect("listed");
    assert_eq!(f.path, "src/a.txt");
    assert_eq!(f.kind, FileKind::File);
    assert_eq!(f.text, NodeStatus::Modified);
    assert_eq!(f.props, PropStatus::Normal);
    assert_eq!(f.changelist.as_deref(), Some("cl"));
    assert!(f.needs_lock);
    assert_eq!(f.size.map(|s| s.0), Some(42));
}

#[test]
fn lock_and_remote_lock_ownership() {
    let mut s = base();
    s.lock = Some(lock("tok"));
    s.lock_is_mine = true;
    s.remote = Some(RemoteStatus {
        out_of_date: true,
        remote_rev: Some(Revnum(9)),
        remote_lock: Some(lock("tok")),
    });
    let f = file_status_from_wc(&s).expect("listed");
    let l = f.lock.expect("lock");
    assert!(l.mine);
    assert_eq!(l.created_at, "1970-01-01T00:00:01Z");
    let r = f.remote.expect("remote");
    assert!(r.out_of_date);
    assert_eq!(r.remote_rev.map(|v| v.0), Some(9));
    let rl = r.remote_lock.expect("remote lock");
    assert!(rl.mine);
    assert_eq!(l.token, None, "lock tokens never cross IPC");
    assert_eq!(rl.token, None, "lock tokens never cross IPC");

    s.remote = Some(RemoteStatus {
        out_of_date: false,
        remote_rev: None,
        remote_lock: Some(lock("other")),
    });
    let f = file_status_from_wc(&s).expect("listed");
    assert!(!f.remote.expect("remote").remote_lock.expect("lock").mine);
}

#[test]
fn status_edge_cases() {
    let mut s = base();
    s.text = S::None;
    assert_eq!(
        file_status_from_wc(&s).expect("listed").text,
        NodeStatus::Unversioned
    );
    s.text = S::Incomplete;
    assert_eq!(
        file_status_from_wc(&s).expect("listed").text,
        NodeStatus::Obstructed
    );
    s.text = S::Excluded;
    assert!(file_status_from_wc(&s).is_none());
    s.text = S::Normal;
    s.kind = NodeKind::Dir;
    assert_eq!(file_status_from_wc(&s).expect("listed").kind, FileKind::Dir);
}
