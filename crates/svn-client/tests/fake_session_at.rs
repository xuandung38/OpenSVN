//! Checkout and update through a fake session rooted below the repository root, the way the
//! engine opens one session per repository root and reparents it to the working-copy URL.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use common::*;
use svn_client::{checkout, update};
use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, RepoUrl, Revnum};

#[tokio::test(flavor = "multi_thread")]
async fn checkout_of_trunk_through_a_session_below_the_root() {
    let root = session();
    root.seed(
        "alice",
        "initial",
        &[
            put("trunk/a.txt", "alpha\n"),
            put("trunk/src/lib.rs", "pub fn f() {}\n"),
            prop("trunk", "svn:ignore", Some("target\n")),
            put("branches/b1/x.txt", "other\n"),
            put("README", "top\n"),
        ],
    )
    .expect("seed");
    let trunk_url = RepoUrl::parse(&format!("{ROOT_URL}/trunk")).unwrap();
    let s = root.at(&trunk_url).expect("session at trunk");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    let sum = checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    assert_eq!(sum.revision, Some(Revnum(1)));
    assert_eq!(read(&wc.join("a.txt")), "alpha\n");
    assert_eq!(read(&wc.join("src/lib.rs")), "pub fn f() {}\n");
    assert!(!wc.join("README").exists());
    assert!(!wc.join("trunk").exists());
    assert!(!wc.join("branches").exists());

    let db = open_db(&wc);
    let a = db.read_base(&rp("a.txt")).expect("read").expect("node");
    assert_eq!(a.repos_relpath.as_deref(), Some("trunk/a.txt"));
    let top = db.read_base(&rp("")).expect("read").expect("node");
    assert_eq!(top.repos_relpath.as_deref(), Some("trunk"));
    assert_eq!(s.session_url(), &trunk_url);

    // A change elsewhere in the repository and one in trunk: update picks up trunk only.
    root.seed(
        "bob",
        "r2",
        &[put("trunk/a.txt", "beta\n"), put("README", "changed\n")],
    )
    .expect("r2");
    update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert_eq!(read(&wc.join("a.txt")), "beta\n");
    assert!(!wc.join("README").exists());
}
