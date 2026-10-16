//! An update of a working copy with switched subtrees against svn 1.15: the reporter sends a
//! switched folder or file with `link_path`, and svn_repos then takes both what the working copy
//! has and the new tree from the link (`update_entry`: "For non-switch operations, follow
//! link_path in the target"). The fake session follows links the same way, so a switched
//! subtree is updated at its own location, with the edits of its branch and never the ones of
//! the folder it sits in. Files, NODES and ACTUAL rows and `svn status` must be identical.
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::{checkout, switch, update};
use svn_types::{CancelToken, Depth, RepoUrl, Revnum};

fn url(rel: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url")
}

/// r1: `trunk` with `X/x.txt`, `Y/y.txt`, `Y/Z/z.txt`, `f.txt`. r2: the branch `branches/b`
/// (copy of trunk@1). r3..: edits on both lines, so a subtree updated from the wrong location
/// shows the other line's text.
fn build(repo: &Repo) -> PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("trunk/f.txt"), "f\n");
    write(&seed.join("trunk/X/x.txt"), "x\n");
    write(&seed.join("trunk/Y/y.txt"), "y\n");
    write(&seed.join("trunk/Y/Z/z.txt"), "z\n");
    std::fs::create_dir_all(seed.join("branches")).expect("mkdir");
    repo.svn(&seed, &["add", "-q", "trunk", "branches"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["copy", "-q", "trunk", "branches/b"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    seed
}

/// r3: different edits of the same files on trunk and on the branch, a file added and a folder
/// deleted on the branch only, a property on the branch's `Y`.
fn edit_both_lines(repo: &Repo, seed: &Path) {
    repo.svn(seed, &["update", "-q"]);
    write(&seed.join("trunk/X/x.txt"), "x\ntrunk r3\n");
    write(&seed.join("trunk/Y/y.txt"), "y\ntrunk r3\n");
    write(&seed.join("trunk/f.txt"), "f\ntrunk r3\n");
    write(&seed.join("branches/b/X/x.txt"), "x\nbranch r3\n");
    write(&seed.join("branches/b/Y/y.txt"), "y\nbranch r3\n");
    write(&seed.join("branches/b/f.txt"), "f\nbranch r3\n");
    write(&seed.join("branches/b/X/new.txt"), "new\n");
    repo.svn(seed, &["add", "-q", "branches/b/X/new.txt"]);
    repo.svn(seed, &["rm", "-q", "branches/b/Y/Z"]);
    repo.svn(seed, &["propset", "-q", "color", "red", "branches/b/Y"]);
    repo.svn(seed, &["commit", "-q", "-m", "r3"]);
}

struct Pair {
    oracle: opensvn_svn_oracle::Oracle,
    repo: Repo,
    theirs: PathBuf,
    ours: PathBuf,
    seed: PathBuf,
    _sb: Sandbox,
}

impl Pair {
    /// Checkouts of `trunk@2` by svn and by the engine (a session at trunk, like the app's).
    async fn new() -> Option<Self> {
        let oracle = oracle()?;
        let repo = Repo::new(&oracle);
        let seed = build(&repo);
        let theirs = repo.base().join("svnwc");
        repo.svn(
            &repo.base(),
            &[
                "checkout",
                "-q",
                &format!("{}/trunk@2", repo.url),
                theirs.to_str().unwrap(),
            ],
        );
        let sb = Sandbox::new();
        let ours = sb.path("wc");
        let fake = repo.fake(None);
        checkout(
            &ctx(),
            &fake.at(&url("trunk")).unwrap(),
            &ours,
            Some(Revnum(2)),
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout");
        Some(Self {
            oracle,
            repo,
            theirs,
            ours,
            seed,
            _sb: sb,
        })
    }

    /// `svn switch ^/branches/b/<path>@2 <path>` in both copies. The engine's session is at
    /// the working copy location of the target (its parent for a file), like the app's.
    async fn switch(&self, path: &str, file: bool) {
        let to = format!("{}/branches/b/{path}@2", self.repo.url);
        self.repo.svn(&self.theirs, &["switch", "-q", &to, path]);
        let fake = self.repo.fake(None);
        switch(
            &ctx(),
            &fake
                .at(&url(if file {
                    "trunk".to_owned()
                } else {
                    format!("trunk/{path}")
                }
                .as_str()))
                .unwrap(),
            &self.ours.join(path),
            &url(&format!("branches/b/{path}")),
            Some(Revnum(2)),
            Depth::Unknown,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("switch");
    }

    /// `svn update` of the whole working copy in both.
    async fn update(&self, rev: Option<u64>) {
        let mut args = vec!["update", "-q"];
        let r;
        if let Some(rev) = rev {
            r = rev.to_string();
            args.extend(["-r", &r]);
        }
        self.repo.svn(&self.theirs, &args);
        let fake = self.repo.fake(None);
        update(
            &ctx(),
            &fake.at(&url("trunk")).unwrap(),
            &self.ours,
            rev.map(Revnum),
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .expect("update");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn update_takes_a_switched_folder_from_its_branch_like_svn() {
    let Some(p) = Pair::new().await else { return };
    p.switch("X", false).await;
    p.switch("Y", false).await;
    edit_both_lines(&p.repo, &p.seed);
    p.update(None).await;
    assert_same(&p.oracle, &p.theirs, &p.ours, "switched folders");
    assert_eq!(read(&p.ours.join("X/x.txt")), "x\nbranch r3\n");
    assert_eq!(read(&p.ours.join("f.txt")), "f\ntrunk r3\n");
    assert!(p.ours.join("X/new.txt").exists());
    assert!(!p.ours.join("Y/Z").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn update_takes_a_switched_file_from_its_branch_like_svn() {
    let Some(p) = Pair::new().await else { return };
    p.switch("f.txt", true).await;
    edit_both_lines(&p.repo, &p.seed);
    p.update(None).await;
    assert_same(&p.oracle, &p.theirs, &p.ours, "switched file");
    assert_eq!(read(&p.ours.join("f.txt")), "f\nbranch r3\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_of_a_switched_folder_below_a_switched_folder_like_svn() {
    let Some(p) = Pair::new().await else { return };
    // `Y` from the branch, and inside it `Y/Z` back from trunk.
    p.switch("Y", false).await;
    let to = format!("{}/trunk/Y/Z@2", p.repo.url);
    p.repo.svn(&p.theirs, &["switch", "-q", &to, "Y/Z"]);
    let fake = p.repo.fake(None);
    switch(
        &ctx(),
        &fake.at(&url("trunk/Y/Z")).unwrap(),
        &p.ours.join("Y/Z"),
        &url("trunk/Y/Z"),
        Some(Revnum(2)),
        Depth::Unknown,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("switch Y/Z");
    edit_both_lines(&p.repo, &p.seed);
    write(&p.seed.join("trunk/Y/Z/z.txt"), "z\ntrunk r4\n");
    p.repo.svn(&p.seed, &["commit", "-q", "-m", "r4"]);
    // r3 first (the branch deleted its `Z`; the working copy's `Y/Z` is trunk's), then HEAD.
    p.update(Some(3)).await;
    assert_same(&p.oracle, &p.theirs, &p.ours, "nested switch at r3");
    p.update(None).await;
    assert_same(&p.oracle, &p.theirs, &p.ours, "nested switch at HEAD");
    assert_eq!(read(&p.ours.join("Y/Z/z.txt")), "z\ntrunk r4\n");
}
