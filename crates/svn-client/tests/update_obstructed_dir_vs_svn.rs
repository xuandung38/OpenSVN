//! Update below a versioned directory replaced on disk by a file or a symlink (#150), compared
//! with svn 1.15: working files, NODES and ACTUAL rows (conflicts included) and `svn status`.
//!
//! svn opens the directory with a tree conflict "local obstruction, incoming dir edit" (its
//! `edit_obstructed`), moves the BASE of the directory and of everything below it on, and leaves
//! the disk alone. A later update skips the conflicted directory, an incoming delete of it keeps
//! it as a local change.
//!
//! One deliberate difference: svn's restore pass follows a symlink to a directory and writes the
//! files it misses behind it (out of the working copy if the link points there). We never follow
//! the link. That case is compared on wc.db only, and the directory behind the link must stay
//! byte-identical.
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};

/// r1: `d` with files, a subdirectory and a property, next to `keep`; r2: changes all over `d`
/// (add, edit, delete, a new subdirectory, a property) and in `keep`; r3: `d` deleted.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("d/in.txt"), "in\n");
    write(&seed.join("d/gone.txt"), "gone\n");
    write(&seed.join("d/sub/s.txt"), "s\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "d", "keep"]);
    repo.svn(&seed, &["propset", "-q", "color", "red", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    write(&seed.join("d/new.txt"), "new\n");
    write(&seed.join("d/sub/s.txt"), "s\ns2\n");
    write(&seed.join("d/sub2/t.txt"), "t\n");
    write(&seed.join("keep/k2.txt"), "k2\n");
    repo.svn(&seed, &["add", "-q", "d/new.txt", "d/sub2", "keep/k2.txt"]);
    repo.svn(&seed, &["rm", "-q", "d/gone.txt"]);
    repo.svn(&seed, &["propset", "-q", "p", "v", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&seed, &["rm", "-q", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
}

/// What replaces a versioned directory on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Obstruction {
    /// `d` replaced by a regular file.
    File,
    /// `d` replaced by a symlink to nothing.
    DanglingLink,
    /// `d` replaced by a symlink to an unversioned file next to it.
    LinkToFile,
    /// `d` replaced by a symlink to its own former content, moved out of the working copy.
    LinkToOutsideDir,
    /// `d` carries a local property change, then is replaced by a file.
    PropEditedFile,
    /// `d/sub` replaced by a symlink to nothing (the obstruction one level down).
    NestedLink,
}

const ALL: [Obstruction; 6] = [
    Obstruction::File,
    Obstruction::DanglingLink,
    Obstruction::LinkToFile,
    Obstruction::LinkToOutsideDir,
    Obstruction::PropEditedFile,
    Obstruction::NestedLink,
];

/// Where `LinkToOutsideDir` moves the directory: next to the working copy, reached through a
/// relative link so both sides have the same link text.
fn outside_of(wc: &Path) -> PathBuf {
    let name = wc.file_name().expect("name").to_string_lossy();
    wc.parent().expect("parent").join(format!("out-{name}"))
}

fn obstruct(repo: &Repo, wc: &Path, how: Obstruction) {
    let d = wc.join("d");
    match how {
        Obstruction::File => {
            fs::remove_dir_all(&d).expect("rm d");
            write(&d, "my file\n");
        }
        Obstruction::DanglingLink => {
            fs::remove_dir_all(&d).expect("rm d");
            symlink("nowhere", &d).expect("link");
        }
        Obstruction::LinkToFile => {
            fs::remove_dir_all(&d).expect("rm d");
            write(&wc.join("tgt"), "target\n");
            symlink("tgt", &d).expect("link");
        }
        Obstruction::LinkToOutsideDir => {
            let outside = outside_of(wc);
            fs::rename(&d, &outside).expect("move d out");
            // An edit behind the link that nothing may touch.
            write(&outside.join("in.txt"), "edited behind the link\n");
            let name = outside.file_name().expect("name").to_string_lossy();
            symlink(format!("../{name}"), &d).expect("link");
        }
        Obstruction::PropEditedFile => {
            repo.svn(wc, &["propset", "-q", "mine", "yes", "d"]);
            fs::remove_dir_all(&d).expect("rm d");
            write(&d, "my file\n");
        }
        Obstruction::NestedLink => {
            fs::remove_dir_all(d.join("sub")).expect("rm d/sub");
            symlink("nowhere", d.join("sub")).expect("link");
        }
    }
}

/// Every entry below `dir` (symlinks not followed): kind, content, mode and mtime.
fn snapshot_outside(dir: &Path) -> BTreeMap<String, (String, Vec<u8>, u32, i64, i64)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (String, Vec<u8>, u32, i64, i64)>) {
        for e in fs::read_dir(dir).expect("read_dir") {
            let path = e.expect("entry").path();
            let rel = path
                .strip_prefix(root)
                .expect("strip")
                .to_string_lossy()
                .into_owned();
            let md = fs::symlink_metadata(&path).expect("md");
            let (kind, content) = if md.file_type().is_symlink() {
                (
                    "link",
                    fs::read_link(&path)
                        .expect("link")
                        .into_os_string()
                        .into_encoded_bytes(),
                )
            } else if md.is_dir() {
                ("dir", Vec::new())
            } else {
                ("file", fs::read(&path).expect("read"))
            };
            out.insert(
                rel,
                (
                    kind.to_owned(),
                    content,
                    md.mode(),
                    md.mtime(),
                    md.mtime_nsec(),
                ),
            );
            if md.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// No `incomplete` row anywhere: the update finished every directory it opened.
fn assert_complete(wc: &Path, what: &str) {
    let rows = nodes_snapshot(wc);
    let incomplete: Vec<_> = rows.iter().filter(|r| r.contains("|incomplete|")).collect();
    assert!(
        incomplete.is_empty(),
        "{what}: incomplete rows left: {incomplete:?}"
    );
}

/// NODES rows of [`nodes_snapshot`] without `translated_size` (the 9th column).
fn without_translated_size(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|r| {
            let mut cols: Vec<&str> = r.split('|').collect();
            cols.remove(8);
            cols.join("|")
        })
        .collect()
}

/// Compares the two working copies. With a link to a directory, svn reads and writes behind
/// the link, so only wc.db is compared.
fn compare(
    oracle: &opensvn_svn_oracle::Oracle,
    theirs: &Path,
    ours: &Path,
    how: Obstruction,
    what: &str,
) {
    assert_complete(ours, what);
    if how == Obstruction::LinkToOutsideDir {
        // svn's restore pass writes the files below `d` it misses behind the link and records
        // their size; we leave them missing. Everything else in NODES is the same.
        assert_eq!(
            without_translated_size(nodes_snapshot(ours)),
            without_translated_size(nodes_snapshot(theirs)),
            "{what}: NODES rows differ"
        );
        assert_eq!(
            actual_snapshot(ours),
            actual_snapshot(theirs),
            "{what}: ACTUAL rows differ"
        );
    } else {
        assert_same(oracle, theirs, ours, what);
    }
}

async fn our_update(s: &svn_ra_api::fake::FakeRaSession, wc: &Path, rev: u64, what: &str) {
    update(
        &ctx(),
        s,
        wc,
        Some(Revnum(rev)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap_or_else(|e| panic!("{what}: update to r{rev}: {e}"));
}

/// Both working copies at r1 with the same obstruction; returns `(theirs, ours)`.
async fn prepare(repo: &Repo, sb: &Sandbox, name: &str, how: Obstruction) -> (PathBuf, PathBuf) {
    let theirs = repo.base().join(format!("svn-{name}"));
    let target = theirs.to_str().expect("utf-8 path");
    repo.svn(
        &repo.base(),
        &["checkout", "-q", "-r", "1", &repo.url, target],
    );
    let ours = sb.path(&format!("svn-{name}"));
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
    obstruct(repo, &theirs, how);
    obstruct(repo, &ours, how);
    (theirs, ours)
}

#[tokio::test(flavor = "multi_thread")]
async fn update_below_an_obstructed_directory_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let fake = repo.fake(None);
    for how in ALL {
        let name = format!("{how:?}").to_lowercase();
        let (theirs, ours) = prepare(&repo, &sb, &name, how).await;
        let outside = outside_of(&ours);
        let before = (how == Obstruction::LinkToOutsideDir).then(|| snapshot_outside(&outside));

        // r2: edits below the obstruction.
        repo.svn(&theirs, &["update", "-q", "-r", "2"]);
        our_update(&fake, &ours, 2, &name).await;
        compare(&oracle, &theirs, &ours, how, &format!("{how:?} r2"));

        // r3: the obstructed directory is deleted in the repository.
        repo.svn(&theirs, &["update", "-q", "-r", "3"]);
        our_update(&fake, &ours, 3, &name).await;
        compare(&oracle, &theirs, &ours, how, &format!("{how:?} r3"));

        if let Some(before) = before {
            assert_eq!(
                snapshot_outside(&outside),
                before,
                "the directory behind the link changed"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_an_obstructed_directory_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let fake = repo.fake(None);
    for how in ALL {
        let name = format!("del-{how:?}").to_lowercase();
        let (theirs, ours) = prepare(&repo, &sb, &name, how).await;
        let outside = outside_of(&ours);
        let before = (how == Obstruction::LinkToOutsideDir).then(|| snapshot_outside(&outside));

        repo.svn(&theirs, &["update", "-q", "-r", "3"]);
        our_update(&fake, &ours, 3, &name).await;
        compare(&oracle, &theirs, &ours, how, &format!("{how:?} r1 to r3"));

        if let Some(before) = before {
            assert_eq!(
                snapshot_outside(&outside),
                before,
                "the directory behind the link changed"
            );
        }
    }
}
