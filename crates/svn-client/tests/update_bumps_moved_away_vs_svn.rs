//! An update that does not touch a locally moved node moves the destination along to the new
//! revision, like svn 1.15 (`bump_moved_away` at the end of `bump_revisions_post_update`): the
//! moved-here rows are based on the revision their source now is. Found over HTTPS by the
//! two-way compat scenario `interop-svn-wc` (`svn info` showed the old revision on the
//! destination after the engine updated an svn working copy).
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};

/// r1: `d/x.txt`, `d/sub/s.txt`, `keep/k.txt`; r2: an edit of `keep/k.txt` only.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("d/x.txt"), "x\n");
    write(&seed.join("d/sub/s.txt"), "s\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "d", "keep"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    write(&seed.join("keep/k.txt"), "k\nedited\n");
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

fn svn_checkout_r1(repo: &Repo, name: &str) -> PathBuf {
    let wc = repo.base().join(name);
    let path = wc.to_str().expect("utf-8 path");
    repo.svn(
        &repo.base(),
        &["checkout", "-q", "-r", "1", &repo.url, path],
    );
    wc
}

async fn our_checkout_r1(repo: &Repo, sb: &Sandbox, name: &str) -> PathBuf {
    let ours = sb.path(name);
    let fake = repo.fake(None);
    checkout(
        &ctx(),
        &fake,
        &ours,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    ours
}

async fn our_update(repo: &Repo, wc: &Path) {
    let fake = repo.fake(None);
    update(&ctx(), &fake, wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_moves_an_untouched_move_destination_along_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let cases: [(&str, &[&str]); 3] = [
        ("file", &["mv", "-q", "d/x.txt", "moved.txt"]),
        ("dir", &["mv", "-q", "d/sub", "sub2"]),
        ("inside", &["mv", "-q", "d/x.txt", "d/sub/x2.txt"]),
    ];
    for (name, mv) in cases {
        let theirs = svn_checkout_r1(&repo, &format!("svn-{name}"));
        let ours = our_checkout_r1(&repo, &sb, &format!("ours-{name}")).await;
        repo.svn(&theirs, mv);
        repo.svn(&ours, mv);
        repo.svn(&theirs, &["update", "-q"]);
        our_update(&repo, &ours).await;
        assert_same(&oracle, &theirs, &ours, name);
    }
}
