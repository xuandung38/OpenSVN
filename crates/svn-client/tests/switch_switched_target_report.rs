//! Updating or switching a target that is already switched (its URL is not its parent's URL
//! plus its name), through a session at the working copy root.
//!
//! - A switched **directory** is its own anchor, like svn's `svn_wc_get_actual_target2`: the
//!   session moves to the directory's URL and the report is a plain `set_path("")`, never a
//!   `link_path`. A place-holder `set_path` followed by a `link_path` for a directory and then
//!   entries below it makes svn_repos read freed memory (`reporter.c`), and mod_dav_svn 1.14 and
//!   1.15 abort the request.
//! - A target below a switched directory is anchored at that directory, the same way.
//! - A switched **file** is reported as svn's crawler does: a place-holder `set_path` carrying
//!   the parent's revision, then its `link_path` (nothing follows a file).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod support;

use common::{ROOT_URL, ctx, put, read, session};
use support::rec_ra::RecRa;
use svn_client::{checkout, switch, update};
use svn_types::{CancelToken, Depth, RepoUrl};

/// The last report.
fn last_report(ra: &RecRa) -> Vec<String> {
    ra.reports.lock().unwrap().last().expect("a report").clone()
}

fn url(rel: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switched_directory_is_its_own_anchor() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("trunk/a.txt", "trunk\n"),
            put("trunk/sub/b.txt", "trunk sub\n"),
            put("branch/a.txt", "branch\n"),
            put("branch/sub/b.txt", "branch sub\n"),
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
    switch(
        &ctx(),
        &ra,
        &wc.join("trunk"),
        &url("branch"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("first switch");
    assert_eq!(read(&wc.join("trunk/a.txt")), "branch\n");
    // Not switched yet: a plain set_path of the target below the root.
    assert_eq!(last_report(&ra)[0], "set_path trunk r1");

    ra.inner
        .seed(
            "a",
            "r2",
            &[
                put("branch/a.txt", "branch 2\n"),
                put("branch/sub/b.txt", "branch sub 2\n"),
            ],
        )
        .expect("r2");
    // Again, now that `trunk` is switched: anchored at it, at the branch URL.
    switch(
        &ctx(),
        &ra,
        &wc.join("trunk"),
        &url("branch"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("second switch of the switched directory");
    let report = last_report(&ra);
    assert_eq!(report[0], "set_path  r1", "{report:?}");
    assert!(
        !report.iter().any(|l| l.starts_with("link_path")),
        "a directory target gets no link_path: {report:?}"
    );
    assert!(
        ra.events()
            .contains(&format!("reparented {ROOT_URL}/branch")),
        "{:?}",
        ra.events()
    );
    assert_eq!(read(&wc.join("trunk/a.txt")), "branch 2\n");

    ra.inner
        .seed("a", "r3", &[put("branch/sub/b.txt", "branch sub 3\n")])
        .expect("r3");
    // An update of the switched directory is reported the same way.
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
    let report = last_report(&ra);
    assert_eq!(report[0], "set_path  r2", "{report:?}");
    assert!(
        !report.iter().any(|l| l.starts_with("link_path")),
        "{report:?}"
    );
    assert_eq!(read(&wc.join("trunk/sub/b.txt")), "branch sub 3\n");

    // A target below the switched directory is anchored at that directory.
    ra.inner
        .seed("a", "r4", &[put("branch/sub/b.txt", "branch sub 4\n")])
        .expect("r4");
    update(
        &ctx(),
        &ra,
        &wc.join("trunk/sub"),
        None,
        Depth::Unknown,
        &cancel,
    )
    .await
    .expect("update below the switched directory");
    let report = last_report(&ra);
    assert_eq!(report[0], "set_path sub r3", "{report:?}");
    assert!(
        !report.iter().any(|l| l.starts_with("link_path")),
        "{report:?}"
    );
    assert_eq!(read(&wc.join("trunk/sub/b.txt")), "branch sub 4\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switched_file_gets_a_place_holder_then_its_link() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[put("trunk/f.txt", "trunk\n"), put("other/f.txt", "other\n")],
    )
    .expect("seed");
    let ra = RecRa::new(s);
    let sb = tempfile::tempdir().unwrap();
    let wc = sb.path().canonicalize().unwrap().join("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &ra, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    switch(
        &ctx(),
        &ra,
        &wc.join("trunk/f.txt"),
        &url("other/f.txt"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("first switch of the file");
    assert_eq!(read(&wc.join("trunk/f.txt")), "other\n");

    ra.inner
        .seed("a", "r2", &[put("other/f.txt", "other 2\n")])
        .expect("r2");
    switch(
        &ctx(),
        &ra,
        &wc.join("trunk/f.txt"),
        &url("other/f.txt"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("second switch of the switched file");
    let report = last_report(&ra);
    assert_eq!(
        report,
        vec![
            "set_path trunk/f.txt r1".to_owned(),
            format!("link_path trunk/f.txt {ROOT_URL}/other/f.txt r1"),
        ]
    );
    assert_eq!(read(&wc.join("trunk/f.txt")), "other 2\n");
}
