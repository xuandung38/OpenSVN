//! Crash-window behaviour of write transactions, driven by the `fail` crate.
//! Run with `cargo test -p svn-wc --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::panic)]

use svn_types::{Depth, NodeKind, RelPath};
use svn_wc::db::{NewWc, WcDb};

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
    scenario.teardown();
}
