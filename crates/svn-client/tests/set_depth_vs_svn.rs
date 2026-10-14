//! `svn update --set-depth` against svn 1.15: the same repository history (replayed into the
//! fake session) and the same local changes, then `svn update --set-depth` on one checkout and
//! `svn_client::set_depth` on the other. Files, NODES and ACTUAL rows and `svn status` must be
//! identical. Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use opensvn_svn_oracle::Oracle;
use svn_client::{checkout, set_depth};
use svn_types::{CancelToken, Depth, ErrorCode};

fn word(d: Depth) -> &'static str {
    match d {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Exclude => "exclude",
        _ => "infinity",
    }
}

/// r1: a tree under `A` with something to crop at every level; r2 changes inside and outside it.
fn build(repo: &Repo) -> PathBuf {
    let seed = repo.checkout("seed");
    for d in ["X1", "X2", "X3", "X4", "X5", "X7", "K", "M", "N", "R"] {
        write(&seed.join(format!("A/{d}/f")), &format!("{d}\n"));
    }
    for f in ["f", "g", "h", "del", "kl", "miss"] {
        write(&seed.join(format!("A/{f}")), &format!("{f}\n"));
    }
    write(&seed.join("A/Y/y"), "y\n");
    write(&seed.join("A/Y/Z/z"), "z\n");
    write(&seed.join("top.txt"), "top\n");
    repo.svn(&seed, &["add", "-q", "A", "top.txt"]);
    repo.svn(&seed, &["propset", "-q", "svn:ignore", "*.o", "A/X3"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    write(&seed.join("A/new.txt"), "new\n");
    write(&seed.join("A/Y/new.txt"), "new y\n");
    write(&seed.join("A/X1/f"), "X1\nr2\n");
    write(&seed.join("top.txt"), "top\nr2\n");
    repo.svn(&seed, &["add", "-q", "A/new.txt", "A/Y/new.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    seed
}

/// Local changes of every kind crop has to respect, made with the real svn in both copies.
fn local_changes(repo: &Repo, wc: &Path) {
    let svn = |args: &[&str]| {
        repo.svn(wc, args);
    };
    write(&wc.join("A/f"), "f\nlocal\n");
    svn(&["propset", "-q", "p", "v", "A/h"]);
    write(&wc.join("A/X2/unv"), "unversioned\n");
    write(&wc.join("A/X3/ign.o"), "ignored\n");
    svn(&["propset", "-q", "p", "v", "A/X4"]);
    svn(&["rm", "-q", "A/X5/f"]);
    svn(&["changelist", "-q", "cl", "A/X7/f"]);
    write(&wc.join("A/Y/Z/z"), "z\nlocal\n");
    svn(&["rm", "-q", "--keep-local", "A/K/f"]);
    fs::remove_dir_all(wc.join("A/M")).expect("rm A/M");
    svn(&["mkdir", "-q", "A/N/added"]);
    svn(&["rm", "-q", "A/R"]);
    svn(&["mkdir", "-q", "A/R"]);
    svn(&["rm", "-q", "A/del"]);
    svn(&["rm", "-q", "--keep-local", "A/kl"]);
    fs::remove_file(wc.join("A/miss")).expect("rm A/miss");
}

struct Pair {
    oracle: Oracle,
    repo: Repo,
    theirs: PathBuf,
    ours: PathBuf,
    _sb: Sandbox,
}

/// Both copies at r1 (`depth`), with `local` applied to each.
async fn pair(depth: Depth, local: impl Fn(&Repo, &Path)) -> Option<Pair> {
    let oracle = oracle()?;
    let repo = Repo::new(&oracle);
    build(&repo);
    let theirs = repo.base().join("svnwc");
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            "-r",
            "1",
            "--depth",
            word(depth),
            &repo.url,
            theirs.to_str().unwrap(),
        ],
    );
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(
        &ctx(),
        &repo.fake(Some(1)),
        &ours,
        None,
        depth,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout r1");
    local(&repo, &theirs);
    local(&repo, &ours);
    assert_same(&oracle, &theirs, &ours, "before");
    Some(Pair {
        oracle,
        repo,
        theirs,
        ours,
        _sb: sb,
    })
}

impl Pair {
    async fn set_depth(&self, target: &str, depth: Depth) {
        let what = format!("--set-depth {} {target}", word(depth));
        let mut args = vec![
            "update",
            "-q",
            "--accept",
            "postpone",
            "--set-depth",
            word(depth),
        ];
        if !target.is_empty() {
            args.push(target);
        }
        self.repo.svn(&self.theirs, &args);
        set_depth(
            &ctx(),
            &self.repo.fake(None),
            &self.ours.join(target),
            None,
            depth,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        if depth == Depth::Exclude {
            // svn 1.15 leaves the removals queued (and the working copy locked) when it crops
            // instead of excluding; `svn cleanup` finishes them. svn_client::set_depth runs them.
            self.repo.svn(&self.theirs, &["cleanup"]);
        }
        assert_same(&self.oracle, &self.theirs, &self.ours, &what);
        // svn keeps working with the result.
        self.repo.svn(&self.ours, &["status"]);
        self.repo.svn(&self.ours, &["cleanup"]);
    }
}

async fn crop_then_grow(depth: Depth) {
    let Some(p) = pair(Depth::Infinity, local_changes).await else {
        return;
    };
    p.set_depth("A", depth).await;
    // Modified files are never removed.
    assert_eq!(read(&p.ours.join("A/f")), "f\nlocal\n");
    assert_eq!(read(&p.ours.join("A/Y/Z/z")), "z\nlocal\n");
    assert!(p.ours.join("A/X2/unv").exists());
    assert!(p.ours.join("A/X3/ign.o").exists());
    assert!(p.ours.join("A/kl").exists());
    p.set_depth("A", Depth::Infinity).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crop_to_empty_then_back_to_infinity() {
    crop_then_grow(Depth::Empty).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crop_to_files_then_back_to_infinity() {
    crop_then_grow(Depth::Files).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crop_to_immediates_then_back_to_infinity() {
    crop_then_grow(Depth::Immediates).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grow_a_sparse_checkout_step_by_step() {
    let Some(p) = pair(Depth::Empty, |_, _| {}).await else {
        return;
    };
    p.set_depth("", Depth::Files).await;
    p.set_depth("", Depth::Immediates).await;
    p.set_depth("A", Depth::Immediates).await;
    p.set_depth("A/Y", Depth::Infinity).await;
    p.set_depth("", Depth::Infinity).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exclude_and_bring_back() {
    let Some(p) = pair(Depth::Infinity, local_changes).await else {
        return;
    };
    // Unmodified: excluded. Modified below: cropped to empty instead.
    p.set_depth("A/X1", Depth::Exclude).await;
    p.set_depth("A/Y", Depth::Exclude).await;
    p.set_depth("A/g", Depth::Exclude).await;
    assert!(!p.ours.join("A/X1").exists());
    assert_eq!(read(&p.ours.join("A/Y/Z/z")), "z\nlocal\n");
    // The parent's depth brings the excluded nodes back.
    p.set_depth("A", Depth::Infinity).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals() {
    let Some(p) = pair(Depth::Infinity, local_changes).await else {
        return;
    };
    let fake = p.repo.fake(None);
    for (target, depth) in [
        ("A/f", Depth::Empty),       // a file has no depth
        ("A/N/added", Depth::Empty), // scheduled for addition
        ("A/R", Depth::Empty),       // replaced
        ("", Depth::Exclude),        // the root cannot be excluded
        ("A/kl", Depth::Exclude),    // scheduled for deletion
    ] {
        let err = set_depth(
            &ctx(),
            &fake,
            &p.ours.join(target),
            None,
            depth,
            &CancelToken::new(),
        )
        .await
        .expect_err(target);
        assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{target}: {err}");
    }
}
