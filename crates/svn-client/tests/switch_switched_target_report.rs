//! Updating or switching a directory that is already switched (its URL is not its parent's URL
//! plus its name), through a session at the working copy root. The report must still open with
//! a `set_path` of the target: svn_repos reads it as a place-holder carrying the anchor's
//! revision, and the `link_path` that follows describes the switched target. The HTTP layer
//! refuses a report that opens with a `link_path` ("update report must start with set_path for
//! the root"), which a second `switch` of the same directory used to send.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod support;

use common::{ROOT_URL, ctx, put, read, session};
use support::rec_ra::RecRa;
use svn_client::{checkout, switch, update};
use svn_types::{CancelToken, Depth, RepoUrl};

/// The first line of the last report, and the whole report.
fn last_report(ra: &RecRa) -> (String, Vec<String>) {
    let reports = ra.reports.lock().unwrap();
    let last = reports.last().expect("a report").clone();
    (last.first().cloned().unwrap_or_default(), last)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switched_target_is_reported_with_a_place_holder_first() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("trunk/a.txt", "trunk\n"),
            put("branch/a.txt", "branch\n"),
            put("other/b.txt", "readme\n"),
        ],
    )
    .expect("seed");
    let ra = RecRa::new(s);
    let sb = tempfile::tempdir().unwrap();
    let wc = sb.path().canonicalize().unwrap().join("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &ra, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    let branch = RepoUrl::parse(&format!("{ROOT_URL}/branch")).unwrap();
    switch(
        &ctx(),
        &ra,
        &wc.join("trunk"),
        &branch,
        None,
        Depth::Unknown,
        &cancel,
    )
    .await
    .expect("first switch");
    assert_eq!(read(&wc.join("trunk/a.txt")), "branch\n");
    // Not switched yet: a plain set_path of the target.
    let (first, _) = last_report(&ra);
    assert_eq!(first, "set_path trunk r1");

    ra.inner
        .seed("a", "r2", &[put("branch/a.txt", "branch 2\n")])
        .expect("r2");
    // Again, now that `trunk` is switched: a place-holder, then the link.
    switch(
        &ctx(),
        &ra,
        &wc.join("trunk"),
        &branch,
        None,
        Depth::Unknown,
        &cancel,
    )
    .await
    .expect("second switch of the switched directory");
    let (first, all) = last_report(&ra);
    assert_eq!(first, "set_path trunk r1", "{all:?}");
    assert_eq!(
        all.get(1).map(String::as_str),
        Some(format!("link_path trunk {ROOT_URL}/branch r1").as_str()),
        "{all:?}"
    );
    assert_eq!(read(&wc.join("trunk/a.txt")), "branch 2\n");

    // An update of the switched directory reports it the same way.
    update(
        &ctx(),
        &ra,
        &wc.join("trunk"),
        None,
        Depth::Unknown,
        &cancel,
    )
    .await
    .expect("update of the switched directory");
    let (first, all) = last_report(&ra);
    assert_eq!(first, "set_path trunk r1", "{all:?}");
    assert!(all[1].starts_with("link_path trunk "), "{all:?}");
}
