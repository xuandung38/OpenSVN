//! Update over an incoming replacement of a file by a directory (`svn rm f; svn mkdir f`)
//! compared with svn 1.15 (#146): working files, NODES and ACTUAL rows (conflicts included) and
//! `svn status`, for each local state of the file.
//!
//! svn handles the delete and the add of the same name as one replacement: an unmodified or
//! missing file becomes the directory; a locally changed file stays as a copy above the new
//! BASE directory with one tree conflict "local file edit, incoming replace with dir".
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};

/// r1: file `f` and a directory next to it; r2: `f` replaced by a directory with a file.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("f"), "orig\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "f", "keep"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["rm", "-q", "f"]);
    write(&seed.join("f/inner.txt"), "inner\n");
    repo.svn(&seed, &["add", "-q", "f"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// The local states of `f` before the update.
#[derive(Debug, Clone, Copy)]
enum Local {
    Clean,
    Edited,
    PropEdited,
    Deleted,
    Missing,
    UnversionedDir,
}

const ALL: [Local; 6] = [
    Local::Clean,
    Local::Edited,
    Local::PropEdited,
    Local::Deleted,
    Local::Missing,
    Local::UnversionedDir,
];

fn prepare(repo: &Repo, wc: &Path, local: Local) {
    match local {
        Local::Clean => {}
        Local::Edited => write(&wc.join("f"), "orig\nmy edit\n"),
        Local::PropEdited => {
            repo.svn(wc, &["propset", "-q", "mine", "yes", "f"]);
        }
        Local::Deleted => {
            repo.svn(wc, &["rm", "-q", "f"]);
        }
        Local::Missing => fs::remove_file(wc.join("f")).expect("rm"),
        Local::UnversionedDir => {
            fs::remove_file(wc.join("f")).expect("rm");
            write(&wc.join("f/mine.txt"), "my data\n");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_file_to_dir_replacement_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    for local in ALL {
        let name = format!("{local:?}").to_lowercase();
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

        repo.svn(&theirs, &["update", "-q"]);
        update(
            &ctx(),
            &repo.fake(None),
            &ours,
            Some(Revnum(2)),
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{local:?}: update: {e}"));
        assert_same(&oracle, &theirs, &ours, &format!("{local:?}"));
    }
}

/// r1: directory `d` with a file; r2: `d` replaced by a file.
fn build_dir_to_file(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("d/in.txt"), "in\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "d", "keep"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["rm", "-q", "d"]);
    write(&seed.join("d"), "now a file\n");
    repo.svn(&seed, &["add", "-q", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// The local states of `d` before the update.
#[derive(Debug, Clone, Copy)]
enum LocalDir {
    Clean,
    ChildEdited,
    ChildUnversioned,
    Deleted,
    Missing,
    UnversionedFile,
}

fn prepare_dir(repo: &Repo, wc: &Path, local: LocalDir) {
    match local {
        LocalDir::Clean => {}
        LocalDir::ChildEdited => write(&wc.join("d/in.txt"), "in\nmy edit\n"),
        LocalDir::ChildUnversioned => write(&wc.join("d/mine.txt"), "my data\n"),
        LocalDir::Deleted => {
            repo.svn(wc, &["rm", "-q", "d"]);
        }
        LocalDir::Missing => fs::remove_dir_all(wc.join("d")).expect("rm"),
        LocalDir::UnversionedFile => {
            fs::remove_dir_all(wc.join("d")).expect("rm");
            write(&wc.join("d"), "my file\n");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_dir_to_file_replacement_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_dir_to_file(&repo);
    let sb = Sandbox::new();
    for local in [
        LocalDir::Clean,
        LocalDir::ChildEdited,
        LocalDir::ChildUnversioned,
        LocalDir::Deleted,
        LocalDir::Missing,
        LocalDir::UnversionedFile,
    ] {
        let name = format!("{local:?}").to_lowercase();
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
        prepare_dir(&repo, &theirs, local);
        prepare_dir(&repo, &ours, local);

        repo.svn(&theirs, &["update", "-q"]);
        update(
            &ctx(),
            &repo.fake(None),
            &ours,
            Some(Revnum(2)),
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{local:?}: update: {e}"));
        assert_same(&oracle, &theirs, &ours, &format!("{local:?}"));
    }
}

/// Like [`build`], but r2 also edits 50 files that the drive visits before `f` and adds one
/// after it: the replacement happens while the update still holds those earlier nodes
/// uncommitted (they are committed in batches), so they must reach wc.db before the tree
/// conflict of the replacement is recorded, and in drive order.
fn build_with_neighbours(repo: &Repo) {
    let seed = repo.checkout("seed");
    for i in 0..50 {
        write(&seed.join(format!("a/{i:02}.txt")), &format!("{i}\n"));
    }
    write(&seed.join("f"), "orig\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "a", "f", "keep"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    for i in 0..50 {
        write(
            &seed.join(format!("a/{i:02}.txt")),
            &format!("{i} edited\n"),
        );
    }
    repo.svn(&seed, &["rm", "-q", "f"]);
    write(&seed.join("f/inner.txt"), "inner\n");
    write(&seed.join("z.txt"), "z\n");
    repo.svn(&seed, &["add", "-q", "f", "z.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_file_to_dir_replacement_amid_other_changes_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_with_neighbours(&repo);
    let sb = Sandbox::new();
    for local in [Local::Clean, Local::Edited, Local::Deleted] {
        let name = format!("{local:?}").to_lowercase();
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

        repo.svn(&theirs, &["update", "-q"]);
        update(
            &ctx(),
            &repo.fake(None),
            &ours,
            Some(Revnum(2)),
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{local:?}: update: {e}"));
        assert_same(&oracle, &theirs, &ours, &format!("{local:?}"));
        assert_eq!(
            fs::read_to_string(ours.join("a/49.txt")).expect("read"),
            "49 edited\n",
            "{local:?}"
        );
    }
}
