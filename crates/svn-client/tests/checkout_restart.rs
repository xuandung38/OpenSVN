//! A checkout killed before `wc.db` held its schema (#137): update reports an interrupted
//! checkout (not "format 0, run svn upgrade"), and checking out again restarts it.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;

use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, ErrorCode, Revnum};

#[tokio::test(flavor = "multi_thread")]
async fn checkout_restarts_over_the_empty_database_of_a_killed_checkout() {
    let s = session();
    s.seed(
        "alice",
        "r1",
        &[put("a.txt", "alpha\n"), put("d/b.txt", "beta\n")],
    )
    .expect("r1");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    // What a checkout of an earlier version left when killed in its first milliseconds.
    fs::create_dir_all(wc.join(".svn/tmp")).unwrap();
    fs::create_dir(wc.join(".svn/pristine")).unwrap();
    fs::write(wc.join(".svn/wc.db"), b"").unwrap();
    fs::write(wc.join("unversioned.txt"), b"user data\n").unwrap();
    let cancel = CancelToken::new();

    let err = update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect_err("not a working copy yet");
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy, "{}", err.message);
    assert!(
        err.message.contains("interrupted checkout"),
        "{}",
        err.message
    );

    let sum = checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout restarts");
    assert_eq!(sum.revision, Some(Revnum(1)));
    assert_eq!(read(&wc.join("a.txt")), "alpha\n");
    assert_eq!(read(&wc.join("d/b.txt")), "beta\n");
    assert_eq!(read(&wc.join("unversioned.txt")), "user data\n");
    let rows = base_rows(&open_db(&wc));
    assert!(rows.iter().all(|r| r.1 == "normal"), "{rows:?}");
    assert!(
        fs::read_dir(wc.join(".svn/tmp")).unwrap().next().is_none(),
        ".svn/tmp not empty"
    );

    // A second checkout into the finished working copy is still refused, and leaves it alone.
    let err = checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect_err("existing working copy");
    assert!(err.message.contains("already holds"), "{}", err.message);
    assert_eq!(read(&wc.join("a.txt")), "alpha\n");
}
