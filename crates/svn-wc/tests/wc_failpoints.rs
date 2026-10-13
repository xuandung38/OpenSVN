//! Crash-window behaviour of write transactions, driven by the `fail` crate.
//! Run with `cargo test -p svn-wc --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::panic)]

use svn_types::{Depth, NodeKind, RelPath};
use svn_wc::db::{FileInfo, NewWc, WcDb};

// One test function: failpoint configuration is process global.
#[test]
fn transaction_failpoints_leave_all_or_nothing_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = WcDb::create(
        tmp.path(),
        &NewWc {
            repos_root_url: "http://h/r".into(),
            repos_uuid: "u".into(),
            repos_relpath: String::new(),
            revision: 0,
            depth: Depth::Infinity,
        },
    )
    .expect("create");
    let a = RelPath::new("a.txt").expect("rel");
    let item = svn_wc::skel::Skel::parse(b"(file-install a.txt)").expect("skel");

    let scenario = fail::FailScenario::setup();
    // Before commit: error surfaces and nothing (rows or work item) is persisted.
    fail::cfg("wc.txn.before_commit", "return").expect("cfg");
    let err = db
        .op_add(&a, NodeKind::File, None, std::slice::from_ref(&item))
        .expect_err("injected");
    assert!(err.message.contains("wc.txn.before_commit"));
    assert!(db.read_node(&a).expect("read").is_none(), "rolled back");
    assert!(db.work_items().expect("items").is_empty());
    fail::remove("wc.txn.before_commit");

    // After commit: the caller sees an error but row and work item are both durable.
    fail::cfg("wc.txn.after_commit", "return").expect("cfg");
    assert!(
        db.op_add(&a, NodeKind::File, None, std::slice::from_ref(&item))
            .is_err()
    );
    fail::remove("wc.txn.after_commit");
    assert!(db.read_node(&a).expect("read").is_some());
    assert_eq!(db.work_items().expect("items").len(), 1);
    drop(db);

    // A reopened handle (fresh process view) sees the same state.
    let db = WcDb::open(tmp.path()).expect("reopen");
    assert!(db.read_node(&a).expect("read").is_some());
    assert_eq!(db.work_items().expect("items").len(), 1);

    // wq_record_and_fetch_next: a failure at either step rolls back delete and fileinfo together.
    let second = svn_wc::skel::Skel::parse(b"(file-remove b.txt)").expect("skel");
    db.queue_work(std::slice::from_ref(&second)).expect("queue");
    let ids: Vec<i64> = db
        .work_items()
        .expect("items")
        .iter()
        .map(|(i, _)| *i)
        .collect();
    assert_eq!(ids.len(), 2);
    let info = FileInfo {
        translated_size: Some(5),
        last_mod_time: Some(7),
    };
    for point in ["wc.wq.after_delete", "wc.wq.after_fileinfo"] {
        fail::cfg(point, "return").expect("cfg");
        let err = db
            .wq_record_and_fetch_next(Some(ids[0]), &[(a.clone(), info)])
            .expect_err("injected");
        assert!(err.message.contains(point), "{}", err.message);
        fail::remove(point);
        assert_eq!(
            db.work_items().expect("items").len(),
            2,
            "{point}: delete rolled back"
        );
        assert_eq!(
            db.read_node(&a)
                .expect("read")
                .expect("node")
                .translated_size,
            None,
            "{point}: fileinfo rolled back"
        );
    }
    let next = db
        .wq_record_and_fetch_next(Some(ids[0]), &[(a.clone(), info)])
        .expect("done")
        .expect("next item");
    assert_eq!(next.0, ids[1]);
    assert_eq!(next.1, second);
    let node = db.read_node(&a).expect("read").expect("node");
    assert_eq!(
        (node.translated_size, node.last_mod_time),
        (Some(5), Some(7))
    );
    assert_eq!(db.work_items().expect("items").len(), 1);
    assert_eq!(
        db.wq_record_and_fetch_next(Some(ids[1]), &[])
            .expect("last"),
        None
    );
    assert!(db.work_items().expect("items").is_empty());

    // First fetch (nothing to complete), several paths, and a path whose node is gone.
    db.queue_work(std::slice::from_ref(&second)).expect("queue");
    let gone = RelPath::new("gone.txt").expect("rel");
    let first = db
        .wq_record_and_fetch_next(None, &[(gone, info), (a.clone(), FileInfo::default())])
        .expect("missing node is ignored")
        .expect("item");
    assert_eq!(first.1, second);
    assert_eq!(
        db.work_items().expect("items").len(),
        1,
        "None completes nothing"
    );
    let node = db.read_node(&a).expect("read").expect("node");
    assert_eq!((node.translated_size, node.last_mod_time), (None, None));
    scenario.teardown();
}
