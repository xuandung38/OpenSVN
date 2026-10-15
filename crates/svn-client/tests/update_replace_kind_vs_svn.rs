//! A node that changes kind on the server, through `update` and through `switch`, compared with
//! svn 1.15: working files, NODES and ACTUAL rows (conflicts included) and `svn status`.
//!
//! One revision replaces `A/f` (a file with a property) by a directory with a property and a
//! child, and `A/d` (a directory with a property and a child) by a file with a property. Each
//! working copy first applies one local state to both nodes: local edits of text or properties,
//! an added or unversioned child, a local replacement, an unversioned obstruction, a local
//! delete, a missing node. svn keeps every local change: as a copy above the new BASE with a tree
//! conflict, or as an unversioned file or directory on disk.
//!
//! The switch moves `A` to `B`, which holds the same tree as `A` after the replacement (the
//! histories are unrelated, so both clients switch with ignore-ancestry semantics).
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, RepoUrl, Revnum};

/// The replaced tree: `f` a directory with a property and a child, `d` a file with a property.
fn write_replaced(repo: &Repo, root: &Path) {
    write(&root.join("f/inner.txt"), "inner\n");
    write(&root.join("f/sub/deep.txt"), "deep\n");
    write(&root.join("d"), "now a file\n");
    repo.svn(root, &["add", "-q", "f", "d"]);
    repo.svn(root, &["propset", "-q", "nf", "dir prop", "f"]);
    repo.svn(root, &["propset", "-q", "svn:ignore", "*.tmp", "f"]);
    repo.svn(
        root,
        &[
            "propset",
            "-q",
            "svn:mime-type",
            "text/x-inner",
            "f/inner.txt",
        ],
    );
    repo.svn(root, &["propset", "-q", "nd", "file prop", "d"]);
    repo.svn(root, &["propset", "-q", "svn:executable", "*", "d"]);
    repo.svn(root, &["propset", "-q", "svn:eol-style", "native", "d"]);
}

/// r1: `A` with file `f` and directory `d` (both with properties) and `B` holding the tree of
/// `A` after the replacement; r2: `A/f` becomes a directory and `A/d` a file.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("A/f"), "orig\n");
    write(&seed.join("A/d/in.txt"), "in\n");
    write(&seed.join("A/d/sub/y.txt"), "y\n");
    write(&seed.join("A/keep/k.txt"), "k\n");
    write(&seed.join("B/keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "A", "B"]);
    repo.svn(&seed, &["propset", "-q", "fp", "file prop", "A/f"]);
    repo.svn(&seed, &["propset", "-q", "svn:needs-lock", "*", "A/f"]);
    repo.svn(&seed, &["propset", "-q", "dp", "dir prop", "A/d"]);
    repo.svn(&seed, &["propset", "-q", "svn:ignore", "*.o", "A/d"]);
    write_replaced(repo, &seed.join("B"));
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    // Delete and add in one commit: one replacement each.
    repo.svn(&seed, &["rm", "-q", "A/f", "A/d"]);
    write_replaced(repo, &seed.join("A"));
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// Local states of `A/f` and `A/d` before the update or switch.
#[derive(Debug, Clone, Copy)]
enum Local {
    Clean,
    /// `f` and `d/sub/y.txt` edited.
    Edited,
    /// A property set on `f` and on `d`.
    PropEdited,
    /// `d/new.txt` added.
    AddedChild,
    /// `d/mine.txt` unversioned.
    UnversionedChild,
    /// `f` replaced by an added directory with an added child, `d` by an added file.
    LocallyReplaced,
    /// `f` replaced on disk by an unversioned directory holding `inner.txt` (the name the
    /// server adds), `d` by an unversioned file.
    Obstructed,
    Deleted,
    /// `svn rm --keep-local`: both stay on disk, unversioned.
    DeletedKeptLocal,
    Missing,
}

const ALL: [Local; 10] = [
    Local::Clean,
    Local::Edited,
    Local::PropEdited,
    Local::AddedChild,
    Local::UnversionedChild,
    Local::LocallyReplaced,
    Local::Obstructed,
    Local::Deleted,
    Local::DeletedKeptLocal,
    Local::Missing,
];

/// Makes the read-only (`svn:needs-lock`) file at `path` writable.
fn writable(path: &Path) {
    let mut perm = fs::metadata(path).expect("stat").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    fs::set_permissions(path, perm).expect("chmod");
}

fn prepare(repo: &Repo, wc: &Path, local: Local) {
    let a = wc.join("A");
    match local {
        Local::Clean => {}
        Local::Edited => {
            writable(&a.join("f"));
            write(&a.join("f"), "orig\nmy edit\n");
            write(&a.join("d/sub/y.txt"), "y\nmy edit\n");
        }
        Local::PropEdited => {
            repo.svn(&a, &["propset", "-q", "mine", "yes", "f", "d"]);
        }
        Local::AddedChild => {
            write(&a.join("d/new.txt"), "new\n");
            repo.svn(&a, &["add", "-q", "d/new.txt"]);
        }
        Local::UnversionedChild => write(&a.join("d/mine.txt"), "my data\n"),
        Local::LocallyReplaced => {
            repo.svn(&a, &["rm", "-q", "f", "d"]);
            write(&a.join("f/mine.txt"), "my data\n");
            write(&a.join("d"), "my file\n");
            repo.svn(&a, &["add", "-q", "f", "d"]);
        }
        Local::Obstructed => {
            fs::remove_file(a.join("f")).expect("rm f");
            write(&a.join("f/inner.txt"), "my inner\n");
            fs::remove_dir_all(a.join("d")).expect("rm d");
            write(&a.join("d"), "my file\n");
        }
        Local::Deleted => {
            repo.svn(&a, &["rm", "-q", "f", "d"]);
        }
        Local::DeletedKeptLocal => {
            repo.svn(&a, &["rm", "-q", "--keep-local", "f", "d"]);
        }
        Local::Missing => {
            fs::remove_file(a.join("f")).expect("rm f");
            fs::remove_dir_all(a.join("d")).expect("rm d");
        }
    }
}

/// Every difference between the two working copies, empty when they match.
fn differences(oracle: &opensvn_svn_oracle::Oracle, theirs: &Path, ours: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{what} differ:\n  ours:   {a}\n  theirs: {b}"));
        }
    };
    check(
        "working files",
        format!("{:?}", tree_snapshot(ours)),
        format!("{:?}", tree_snapshot(theirs)),
    );
    check(
        "NODES rows",
        nodes_snapshot(ours).join("\n          "),
        nodes_snapshot(theirs).join("\n          "),
    );
    check(
        "ACTUAL rows",
        actual_snapshot(ours).join("\n          "),
        actual_snapshot(theirs).join("\n          "),
    );
    check(
        "svn status",
        svn_status(oracle, ours, &[]).replace('\n', "\n          "),
        svn_status(oracle, theirs, &[]).replace('\n', "\n          "),
    );
    out
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Update,
    /// `svn update A/f A/d`: each replaced node is the update target.
    UpdateTargets,
    Switch,
}

async fn run(op: Op) {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let mut failures = Vec::new();
    for local in ALL {
        let name = format!("{op:?}-{local:?}").to_lowercase();
        let theirs = repo.base().join(format!("svn-{name}"));
        let target = theirs.to_str().expect("utf-8 path");
        repo.svn(
            &repo.base(),
            &["checkout", "-q", "-r", "1", &repo.url, target],
        );
        let ours = sb.path(&name);
        checkout(
            &ctx(),
            &repo.fake(Some(1)),
            &ours,
            None,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout r1");
        prepare(&repo, &theirs, local);
        prepare(&repo, &ours, local);

        let fake = repo.fake(None);
        match op {
            Op::Update => {
                repo.svn(&theirs, &["update", "-q"]);
                update(
                    &ctx(),
                    &fake,
                    &ours,
                    Some(Revnum(2)),
                    Depth::Unknown,
                    &CancelToken::new(),
                )
                .await
                .unwrap_or_else(|e| panic!("{local:?}: update: {e}"));
            }
            Op::UpdateTargets => {
                repo.svn(&theirs, &["update", "-q", "A/f", "A/d"]);
                for t in ["A/f", "A/d"] {
                    update(
                        &ctx(),
                        &fake,
                        &ours.join(t),
                        Some(Revnum(2)),
                        Depth::Unknown,
                        &CancelToken::new(),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{local:?}: update {t}: {e}"));
                }
            }
            Op::Switch => {
                let svn_url = format!("{}/B", repo.url);
                repo.svn(
                    &theirs,
                    &["switch", "-q", "--ignore-ancestry", "-r2", &svn_url, "A"],
                );
                let url = RepoUrl::parse(&format!("{ROOT_URL}/B")).expect("url");
                svn_client::switch(
                    &ctx(),
                    &fake,
                    &ours.join("A"),
                    &url,
                    Some(Revnum(2)),
                    Depth::Unknown,
                    &CancelToken::new(),
                )
                .await
                .unwrap_or_else(|e| panic!("{local:?}: switch: {e}"));
            }
        }
        for d in differences(&oracle, &theirs, &ours) {
            failures.push(format!("{op:?} {local:?}: {d}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_kind_replacements_by_update_match_svn() {
    run(Op::Update).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_kind_replacements_of_update_targets_match_svn() {
    run(Op::UpdateTargets).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_kind_replacements_by_switch_match_svn() {
    run(Op::Switch).await;
}
