//! Crash windows of an update drive, driven by the `fail` crate: an injected failure leaves a
//! working copy that the next update completes, with no leftovers in `.svn/tmp`.
//! Run with `cargo test -p svn-client --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};

fn tmp_entries(wc: &Path) -> Vec<String> {
    match std::fs::read_dir(wc.join(".svn/tmp")) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// r1 checked out, `a.txt` edited locally on its first line, r2 edits its last line (a clean
/// merge) and changes `b.txt`.
async fn setup() -> (svn_ra_api::fake::FakeRaSession, Sandbox, std::path::PathBuf) {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[put("a.txt", "one\ntwo\nthree\n"), put("b.txt", "b\n")],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &s,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    std::fs::write(wc.join("a.txt"), "ONE\ntwo\nthree\n").expect("edit");
    s.seed(
        "a",
        "r2",
        &[put("a.txt", "one\ntwo\nTHREE\n"), put("b.txt", "b2\n")],
    )
    .expect("seed");
    (s, sb, wc)
}

async fn resume_and_check(s: &svn_ra_api::fake::FakeRaSession, wc: &Path, what: &str) {
    let sum = update(&ctx(), s, wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .unwrap_or_else(|e| panic!("{what}: resumed update failed: {e}"));
    assert_eq!(sum.revision, Some(Revnum(2)), "{what}");
    assert_eq!(
        read(&wc.join("a.txt")),
        "ONE\ntwo\nTHREE\n",
        "{what}: merged"
    );
    assert_eq!(read(&wc.join("b.txt")), "b2\n", "{what}");
    let db = open_db(wc);
    let rows = base_rows(&db);
    assert!(
        rows.iter().all(|r| r.1 == "normal" && r.3 == 2),
        "{what}: {rows:?}"
    );
    assert!(db.work_items().expect("queue").is_empty(), "{what}");
    assert!(db.conflict(&rp("a.txt")).expect("read").is_none(), "{what}");
    assert_eq!(tmp_entries(wc), Vec::<String>::new(), "{what}: .svn/tmp");
}

// One test function: failpoint configuration is process global.
#[tokio::test(flavor = "multi_thread")]
async fn update_failpoints_leave_a_resumable_working_copy() {
    let scenario = fail::FailScenario::setup();

    // 1. Merge done, transaction not yet written: nothing of a.txt changes and the merge
    //    result does not leak into .svn/tmp.
    {
        let (s, _sb, wc) = setup().await;
        fail::cfg("update.after_merge3_before_txn", "return").expect("cfg");
        let err = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
            .await
            .expect_err("injected");
        fail::remove("update.after_merge3_before_txn");
        assert!(err.message.contains("after_merge3_before_txn"), "{err}");
        let db = open_db(&wc);
        let a = db.read_base(&rp("a.txt")).expect("read").expect("a");
        assert_eq!(a.revision, Some(1), "BASE of a.txt untouched");
        assert_eq!(read(&wc.join("a.txt")), "ONE\ntwo\nthree\n");
        let root = db.read_base(&rp("")).expect("read").expect("root");
        assert_eq!(
            root.presence,
            svn_wc::db::Presence::Incomplete,
            "the root stays incomplete until the drive completes"
        );
        assert_eq!(
            tmp_entries(&wc),
            Vec::<String>::new(),
            "merge temp file removed"
        );
        drop(db);
        resume_and_check(&s, &wc, "after_merge3_before_txn").await;
    }

    // 2. Transaction committed, queue not run: the driver completes the queued work before it
    //    returns the error, so the disk already matches the new BASE.
    {
        let (s, _sb, wc) = setup().await;
        fail::cfg("update.after_txn_before_queue", "1*return").expect("cfg");
        let err = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
            .await
            .expect_err("injected");
        fail::remove("update.after_txn_before_queue");
        assert!(err.message.contains("after_txn_before_queue"), "{err}");
        let db = open_db(&wc);
        assert!(
            db.work_items().expect("queue").is_empty(),
            "queue completed"
        );
        let a = db.read_base(&rp("a.txt")).expect("read").expect("a");
        assert_eq!(a.revision, Some(2), "committed node stays committed");
        assert_eq!(read(&wc.join("a.txt")), "ONE\ntwo\nTHREE\n");
        drop(db);
        resume_and_check(&s, &wc, "after_txn_before_queue").await;
    }

    scenario.teardown();
}
