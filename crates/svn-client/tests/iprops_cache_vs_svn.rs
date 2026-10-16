//! The inherited-properties cache (`NODES.inherited_props`) after checkout, update and switch,
//! compared with svn 1.15: the working copy root and switched nodes cache what their parents
//! outside the working copy pass down; other nodes have no cache. svn 1.14 and TortoiseSVN read
//! `svn:global-ignores` and `svn:auto-props` of the repository from there.
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::{checkout, switch, update};
use svn_ra_api::RaSession;
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, RepoUrl};
use svn_wc::db::WcDb;

/// r1: `trunk/src/a.c`, `branches/b/src/a.c` (no copy: the fake replay has none, so svn
/// switches with `--ignore-ancestry`); r2: properties on `/`, `trunk` and `branches/b`.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("trunk/src/a.c"), "a\n");
    write(&seed.join("branches/b/src/a.c"), "a\n");
    repo.svn(&seed, &["add", "-q", "trunk", "branches"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(
        &seed,
        &["propset", "-q", "svn:global-ignores", "*.bak", "."],
    );
    repo.svn(&seed, &["propset", "-q", "custom:team", "core", "trunk"]);
    repo.svn(
        &seed,
        &["propset", "-q", "custom:team", "branch", "branches/b"],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// Every cached node: `(local relpath, cache)` with the cache rendered sorted.
fn caches(wc: &Path) -> Vec<(String, String)> {
    let db = WcDb::open(wc).expect("open");
    let mut out = Vec::new();
    for rel in db.iprop_cached_below(&rp("")).expect("list") {
        let mut items: Vec<String> = db
            .inherited_props(&rel)
            .expect("read")
            .expect("cached")
            .into_iter()
            .map(|(path, mut props)| {
                props.sort();
                format!("{}={props:?}", String::from_utf8_lossy(&path))
            })
            .collect();
        items.sort();
        out.push((rel.as_str().to_owned(), items.join(";")));
    }
    out
}

/// The fake repository's URL of `path`.
fn furl(fake: &FakeRaSession, path: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{}/{path}", fake.repos_root())).expect("url")
}

fn svn_checkout(repo: &Repo, name: &str, url: &str) -> PathBuf {
    let wc = repo.base().join(name);
    repo.svn(
        &repo.base(),
        &["checkout", "-q", url, wc.to_str().expect("utf-8 path")],
    );
    wc
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_update_and_switch_cache_inherited_props_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let url = |p: &str| format!("{}/{p}", repo.url);

    // A sub-tree checkout caches / and /trunk on its root.
    let theirs = svn_checkout(&repo, "svn-src", &url("trunk/src"));
    let ours = sb.path("ours-src");
    let root = repo.fake(None);
    let fake = root.at(&furl(&root, "trunk/src")).expect("session");
    checkout(&ctx(), &fake, &ours, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    assert_eq!(caches(&ours), caches(&theirs), "checkout of trunk/src");
    assert_eq!(caches(&ours).len(), 1);

    // A switched sub-tree caches its own location's, and an update keeps both caches.
    let theirs = svn_checkout(&repo, "svn-trunk", &url("trunk"));
    let ours = sb.path("ours-trunk");
    let fake = root.at(&furl(&root, "trunk")).expect("session");
    checkout(&ctx(), &fake, &ours, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    repo.svn(
        &theirs,
        &[
            "switch",
            "-q",
            "--ignore-ancestry",
            &url("branches/b/src"),
            "src",
        ],
    );
    let target = furl(&root, "branches/b/src");
    switch(
        &ctx(),
        &fake,
        &ours.join("src"),
        &target,
        None,
        Depth::Unknown,
        false,
        &cancel,
    )
    .await
    .expect("switch");
    assert_eq!(caches(&ours), caches(&theirs), "switch of src");
    assert_eq!(caches(&ours).len(), 2);
    repo.svn(&theirs, &["update", "-q"]);
    update(&ctx(), &fake, &ours, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert_eq!(caches(&ours), caches(&theirs), "update");

    // Switched back: the sub-tree is not switched any more and loses its cache.
    repo.svn(
        &theirs,
        &[
            "switch",
            "-q",
            "--ignore-ancestry",
            &url("trunk/src"),
            "src",
        ],
    );
    let back = furl(&root, "trunk/src");
    switch(
        &ctx(),
        &fake,
        &ours.join("src"),
        &back,
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("switch back");
    assert_eq!(caches(&ours), caches(&theirs), "switch back");
    assert_eq!(caches(&ours).len(), 1);
}
