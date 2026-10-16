//! The filtered status walk (`local_status_of_paths`) and the remote check built on it
//! (`status_remote_changes`) report exactly what the full walk reports for the same paths, and
//! the lazily decoded properties give the same property-related fields.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;

use common::*;
use svn_client::status::local_status_of_paths;
use svn_client::{StatusOptions, checkout, status, status_remote, status_remote_changes};
use svn_types::{CancelToken, Depth, RelPath, WcStatus};

fn all() -> StatusOptions {
    StatusOptions {
        show_unmodified: true,
        ..StatusOptions::default()
    }
}

async fn fixture() -> (svn_ra_api::fake::FakeRaSession, Sandbox, std::path::PathBuf) {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("top.txt", "t\n"),
            put("d1/a.txt", "a\n"),
            put("d1/b.bin", "b\n"),
            put("d1/sub/c.txt", "c\n"),
            put("d2/e.txt", "e\n"),
            put("d2/locked.txt", "l\n"),
            prop(
                "d1/b.bin",
                "svn:mime-type",
                Some("application/octet-stream"),
            ),
            prop("d2/locked.txt", "svn:needs-lock", Some("*")),
            prop("d1/a.txt", "svn:eol-style", Some("native")),
            prop("d1", "svn:ignore", Some("*.tmp")),
        ],
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
    (s, sb, wc)
}

#[tokio::test(flavor = "multi_thread")]
async fn filtered_walk_reports_what_the_full_walk_reports_for_those_paths() {
    let (_s, _sb, wc) = fixture().await;
    std::fs::write(wc.join("d1/sub/c.txt"), "changed\n").unwrap();
    std::fs::write(wc.join("d1/new.txt"), "n\n").unwrap();
    let full = status(&ctx(), &wc, &all(), &CancelToken::new())
        .await
        .unwrap();
    let mime = full.iter().find(|s| s.path.as_str() == "d1/b.bin").unwrap();
    assert!(mime.binary, "svn:mime-type read from the undecoded row");
    let lock = full
        .iter()
        .find(|s| s.path.as_str() == "d2/locked.txt")
        .unwrap();
    assert!(lock.needs_lock);
    let wanted: BTreeSet<RelPath> = [
        "d1/sub/c.txt",
        "d1/b.bin",
        "d2/locked.txt",
        "d1/new.txt",
        "gone.txt",
    ]
    .iter()
    .map(|p| rp(p))
    .collect();
    let db = open_db(&wc);
    let got = local_status_of_paths(
        &db,
        &RelPath::root(),
        &all(),
        None,
        &wanted,
        &CancelToken::new(),
    )
    .unwrap();
    let expect: Vec<WcStatus> = full
        .into_iter()
        .filter(|s| wanted.contains(&s.path))
        .collect();
    assert_eq!(got, expect);
    assert_eq!(got.len(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_changes_match_the_full_remote_status() {
    let (s, _sb, wc) = fixture().await;
    std::fs::write(wc.join("d2/e.txt"), "local\n").unwrap();
    s.seed(
        "a",
        "r2",
        &[
            put("d1/a.txt", "a2\n"),
            put("d2/new.txt", "x\n"),
            del("top.txt"),
        ],
    )
    .expect("seed");
    let opts = all();
    let mut full: Vec<WcStatus> = status_remote(&ctx(), &s, &wc, &opts, &CancelToken::new())
        .await
        .unwrap()
        .into_iter()
        .filter(|st| st.remote.is_some())
        .collect();
    let mut got = status_remote_changes(&ctx(), &s, &wc, &opts, &CancelToken::new())
        .await
        .unwrap();
    full.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    got.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    assert_eq!(got, full);
    let paths: Vec<&str> = got.iter().map(|st| st.path.as_str()).collect();
    assert!(paths.contains(&"d1/a.txt"), "{paths:?}");
    assert!(paths.contains(&"d2/new.txt"), "{paths:?}");
    assert!(paths.contains(&"top.txt"), "{paths:?}");
    assert!(!paths.contains(&"d2/e.txt"), "{paths:?}");
}
