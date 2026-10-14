//! An update that resumes a working copy left by an interrupted one (#151), compared with svn
//! 1.15: the root and `d` are `incomplete` at r2 with `d`'s children still at r1, as a killed
//! update leaves them. Both working copies get the same wc.db rows, then both clients update.
//! Compared: working files, NODES and ACTUAL rows, `svn status`, and our own status.
//!
//! The fake RA now sends what svn's server sends for such a report: `delete_entry` for an
//! `incomplete` directory the repository deleted, `open_directory` with every property for one
//! it still has. Before, the fake sent nothing for the deleted directory and the update left a
//! phantom `d@N incomplete`, and the engine had never seen these drives: a deleted `incomplete`
//! directory that holds a local change (an edit, or an obstruction) is kept as a local copy, laid
//! out like svn's (the children at another revision are copies of their own, the copy of `d`
//! stays `incomplete`).
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::{StatusOptions, checkout, status, update};
use svn_types::{CancelToken, Depth, NodeStatus, PropStatus, Revnum, WcStatus};

/// r1: `d` with files, a subdirectory and properties, next to `keep`; r2: a file added to `d`,
/// one of its properties deleted and the other changed, a file added to `keep`; r3: `d` deleted.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("d/in.txt"), "in\n");
    write(&seed.join("d/sub/s.txt"), "s\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "d", "keep"]);
    repo.svn(&seed, &["propset", "-q", "color", "red", "d"]);
    repo.svn(&seed, &["propset", "-q", "size", "big", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    write(&seed.join("d/new.txt"), "new\n");
    write(&seed.join("keep/k2.txt"), "k2\n");
    repo.svn(&seed, &["add", "-q", "d/new.txt", "keep/k2.txt"]);
    repo.svn(&seed, &["propdel", "-q", "color", "d"]);
    repo.svn(&seed, &["propset", "-q", "size", "small", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&seed, &["rm", "-q", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
}

/// The rows a killed update to r2 leaves: the root and `d` marked `incomplete` at r2 (the update
/// marks a directory before it changes anything below it), everything else still at r1.
fn interrupt(wc: &Path) {
    let db = open_db(wc);
    db.with_write(|tx| {
        let n = tx
            .execute(
                "UPDATE nodes SET presence = 'incomplete', revision = 2 \
                 WHERE local_relpath IN ('', 'd') AND op_depth = 0",
                [],
            )
            .expect("update");
        assert_eq!(n, 2);
        Ok(())
    })
    .expect("interrupt");
}

/// Every row hangs below a parent row, and no BASE row is `incomplete`: the update finished.
fn assert_finished(wc: &Path, what: &str) {
    let rows = nodes_snapshot(wc);
    let paths: BTreeSet<&str> = rows
        .iter()
        .map(|r| r.split('|').next().expect("relpath"))
        .collect();
    for p in &paths {
        let parent = p.rsplit_once('/').map_or("", |(parent, _)| parent);
        assert!(
            p.is_empty() || paths.contains(parent),
            "{what}: '{p}' has no parent row: {rows:?}"
        );
    }
    let incomplete: Vec<_> = rows
        .iter()
        .filter(|r| r.contains("|0|incomplete|"))
        .collect();
    assert!(
        incomplete.is_empty(),
        "{what}: incomplete BASE rows left: {incomplete:?}"
    );
}

fn letters(s: &WcStatus) -> String {
    let c0 = match s.text {
        NodeStatus::Added => 'A',
        NodeStatus::Deleted => 'D',
        NodeStatus::Modified => 'M',
        NodeStatus::Replaced => 'R',
        NodeStatus::Missing | NodeStatus::Incomplete => '!',
        NodeStatus::Obstructed => '~',
        NodeStatus::Unversioned => '?',
        NodeStatus::Conflicted => 'C',
        _ => ' ',
    };
    let c1 = match s.props {
        PropStatus::Modified => 'M',
        PropStatus::Conflicted => 'C',
        _ => ' ',
    };
    let path = if s.path.is_root() {
        ".".to_owned()
    } else {
        s.path.to_string()
    };
    format!("{c0}{c1} {path}")
}

/// Our status against svn's, as `XY path` lines (tree conflict details left out). We report
/// every node of a local copy as added, svn only the root of each copy (the others show `+`):
/// a line of ours that svn does not list is fine when it is `A` below a root svn lists as `A`.
async fn assert_same_status(
    oracle: &opensvn_svn_oracle::Oracle,
    theirs: &Path,
    ours: &Path,
    what: &str,
) {
    let mut svn: Vec<String> = svn_status(oracle, theirs, &[])
        .lines()
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("  ")
                && !l.starts_with("Summary")
                && !l.trim_start().starts_with('>')
        })
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            format!("{}{} {}", c[0], c[1], &l[8..])
        })
        .collect();
    svn.sort();
    let mut mine: Vec<String> =
        status(&ctx(), ours, &StatusOptions::default(), &CancelToken::new())
            .await
            .unwrap_or_else(|e| panic!("{what}: status: {e}"))
            .iter()
            .filter(|s| {
                !s.path.is_root()
                    || s.text != NodeStatus::Normal
                    || matches!(s.props, PropStatus::Modified | PropStatus::Conflicted)
            })
            .map(letters)
            .collect();
    mine.sort();
    let roots: Vec<&str> = svn.iter().filter_map(|l| l.strip_prefix("A  ")).collect();
    mine.retain(|l| {
        svn.contains(l)
            || !l
                .strip_prefix("A  ")
                .is_some_and(|p| roots.iter().any(|r| p.starts_with(&format!("{r}/"))))
    });
    assert_eq!(mine, svn, "{what}: status differs");
}

/// Both working copies at r1, interrupted; `local` changes both the same way first.
async fn prepare(repo: &Repo, sb: &Sandbox, name: &str, local: fn(&Path)) -> (PathBuf, PathBuf) {
    let theirs = repo.base().join(format!("svn-{name}"));
    let target = theirs.to_str().expect("utf-8 path");
    repo.svn(
        &repo.base(),
        &["checkout", "-q", "-r", "1", &repo.url, target],
    );
    let ours = sb.path(name);
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
    for wc in [&theirs, &ours] {
        local(wc);
        interrupt(wc);
    }
    (theirs, ours)
}

async fn our_update(repo: &Repo, wc: &Path, rev: u64) {
    update(
        &ctx(),
        &repo.fake(None),
        wc,
        Some(Revnum(rev)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap_or_else(|e| panic!("update to r{rev}: {e}"));
}

/// A local change made before the interruption.
type Local = fn(&Path);

fn untouched(_: &Path) {}

#[cfg(unix)]
fn dangling_link(wc: &Path) {
    fs::remove_dir_all(wc.join("d")).expect("rm d");
    std::os::unix::fs::symlink("nowhere", wc.join("d")).expect("link");
}

fn edited(wc: &Path) {
    write(&wc.join("d/in.txt"), "in\nmy edit\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn resumed_update_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let mut cases: Vec<(&str, Local)> = vec![("plain", untouched), ("edited", edited)];
    #[cfg(unix)]
    cases.push(("link", dangling_link));
    for (name, local) in cases {
        // Resumed to r2: `d` is completed, its properties are the server's, BASE moves on (or
        // stays below the obstruction).
        let what = format!("{name}: resumed to r2");
        let (theirs, ours) = prepare(&repo, &sb, &format!("{name}-r2"), local).await;
        repo.svn(&theirs, &["update", "-q", "-r", "2"]);
        our_update(&repo, &ours, 2).await;
        assert_same(&oracle, &theirs, &ours, &what);
        assert_same_status(&oracle, &theirs, &ours, &what).await;
        assert_finished(&ours, &what);

        // Resumed straight to r3: the deleted `incomplete` directory goes away, or stays as a
        // conflicted local copy when it holds a local change.
        let what = format!("{name}: resumed to r3");
        let (theirs, ours) = prepare(&repo, &sb, &format!("{name}-r3"), local).await;
        repo.svn(&theirs, &["update", "-q", "-r", "3"]);
        our_update(&repo, &ours, 3).await;
        assert_same(&oracle, &theirs, &ours, &what);
        assert_same_status(&oracle, &theirs, &ours, &what).await;
        assert_finished(&ours, &what);
    }
}
