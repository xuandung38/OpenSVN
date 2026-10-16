//! An incoming delete of a locally changed directory (#159, #160, #161), compared with svn 1.15:
//! working files, NODES and ACTUAL rows (conflicts included) and `svn status`.
//!
//! svn keeps such a directory as a copy of its old BASE with a tree conflict "local dir edit,
//! incoming dir delete" (`delete_entry` with `keep_as_working`, then `make_copy_txn`). Three
//! details of that path are checked here:
//!
//! - a local delete or move of something *below* the directory is a local change too (svn's
//!   `svn_wc__db_has_db_mods` counts every working row of the subtree);
//! - a file external below the directory is not part of the copy (`STMT_SELECT_OP_DEPTH_CHILDREN`
//!   skips `file_external` rows): its BASE row goes with the rest of BASE;
//! - a `server-excluded` node below the directory makes svn refuse the copy, and with it the
//!   whole update (`catch_copy_of_server_excluded`), leaving the directory as it was.
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};
use svn_wc::db::WcDb;

/// r1: `d` (files, a subdirectory, a file that may later be marked `server-excluded`) and
/// `keep`; r2: a file external `d/fext` of `keep/k.txt`; r3: `d` deleted.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("d/x.txt"), "x\n");
    write(&seed.join("d/y.txt"), "y\n");
    write(&seed.join("d/secret.txt"), "secret\n");
    write(&seed.join("d/sub/s.txt"), "s\n");
    write(&seed.join("keep/k.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "d", "keep"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(
        &seed,
        &["propset", "-q", "svn:externals", "^/keep/k.txt fext", "d"],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&seed, &["rm", "-q", "d"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
}

/// svn checkout at `rev`, run by the real svn; tolerates the warning exit of an update whose
/// externals pass failed.
fn svn_checkout(repo: &Repo, name: &str, rev: u64) -> PathBuf {
    let wc = repo.base().join(name);
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            "-r",
            &rev.to_string(),
            &repo.url,
            wc.to_str().expect("utf-8 path"),
        ],
    );
    wc
}

/// `svn update -r <rev>` by the real svn; returns whether it succeeded and its stderr.
fn svn_update(repo: &Repo, wc: &Path, rev: u64) -> (bool, String) {
    let out = repo
        .oracle
        .svn()
        .args(["update", "-q", "-r", &rev.to_string()])
        .current_dir(wc)
        .output()
        .expect("svn update");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn our_checkout(repo: &Repo, sb: &Sandbox, name: &str, rev: u64) -> PathBuf {
    let ours = sb.path(name);
    let fake = repo.fake(None);
    let opener = FakeOpener::new(vec![fake.clone()]);
    checkout(
        &ext_ctx(opener),
        &fake,
        &ours,
        Some(Revnum(rev)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    ours
}

async fn our_update(
    repo: &Repo,
    wc: &Path,
    rev: u64,
) -> svn_types::Result<svn_client::UpdateSummary> {
    let fake = repo.fake(None);
    let opener = FakeOpener::new(vec![fake.clone()]);
    update(
        &ext_ctx(opener),
        &fake,
        wc,
        Some(Revnum(rev)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
}

/// Both working copies at `rev`, with `change` applied to each; returns `(theirs, ours)`.
async fn prepare(
    repo: &Repo,
    sb: &Sandbox,
    name: &str,
    rev: u64,
    change: impl Fn(&Path),
) -> (PathBuf, PathBuf) {
    let theirs = svn_checkout(repo, &format!("svn-{name}"), rev);
    let ours = our_checkout(repo, sb, &format!("svn-{name}"), rev).await;
    change(&theirs);
    change(&ours);
    (theirs, ours)
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_over_a_descendant_delete_or_move_keeps_a_copy_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    type Change = fn(&Repo, &Path);
    let cases: [(&str, Change); 4] = [
        ("rm-file", |r, wc| {
            r.svn(wc, &["rm", "-q", "d/x.txt"]);
        }),
        ("rm-dir", |r, wc| {
            r.svn(wc, &["rm", "-q", "d/sub"]);
        }),
        ("mv-out", |r, wc| {
            r.svn(wc, &["mv", "-q", "d/sub/s.txt", "keep/"]);
        }),
        ("mv-inside", |r, wc| {
            r.svn(wc, &["mv", "-q", "d/y.txt", "d/sub/moved.txt"]);
        }),
    ];
    for (name, change) in cases {
        let (theirs, ours) = prepare(&repo, &sb, name, 1, |wc| change(&repo, wc)).await;
        let (ok, err) = svn_update(&repo, &theirs, 3);
        assert!(ok, "{name}: svn update failed: {err}");
        our_update(&repo, &ours, 3)
            .await
            .unwrap_or_else(|e| panic!("{name}: update: {e}"));
        let st = svn_status(&oracle, &theirs, &[]);
        assert!(st.contains("A  +  C d"), "{name}: svn kept no copy:\n{st}");
        assert_same(&oracle, &theirs, &ours, name);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_keeps_no_copy_of_a_file_external_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let fext_row = |wc: &Path| {
        let db = WcDb::open(wc).expect("open");
        db.read_base(&rp("d/fext"))
            .expect("read")
            .map(|n| n.file_external)
    };
    // The second case also edits the file external: the edit must stay on disk.
    for edit_fext in [false, true] {
        let name = if edit_fext { "fext-edited" } else { "fext" };
        let (theirs, ours) = prepare(&repo, &sb, name, 2, |wc| {
            write(&wc.join("d/x.txt"), "x\nedited\n");
            if edit_fext {
                write(&wc.join("d/fext"), "k\nmine\n");
            }
        })
        .await;
        assert_eq!(fext_row(&theirs), Some(true), "svn: no file external");
        assert_eq!(fext_row(&ours), Some(true), "ours: no file external");

        // svn's externals pass then fails on the unversioned file left at `d/fext`; only the
        // update of the delete itself is compared.
        let _ = svn_update(&repo, &theirs, 3);
        let _ = our_update(&repo, &ours, 3).await;
        assert_same(&oracle, &theirs, &ours, name);
        assert!(
            !nodes_snapshot(&ours)
                .iter()
                .any(|r| r.starts_with("d/fext|")),
            "{name}: the file external was copied"
        );
        let want = if edit_fext { "k\nmine\n" } else { "k\n" };
        assert_eq!(read(&ours.join("d/fext")), want, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_refuses_to_copy_a_server_excluded_node_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    // What an authz rule the server applied at checkout leaves behind (svn's `absent`).
    let exclude = |wc: &Path| {
        write(&wc.join("d/x.txt"), "x\nedited\n");
        let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).expect("open wc.db");
        let n = c
            .execute(
                "UPDATE nodes SET presence = 'server-excluded', checksum = NULL, \
                 properties = NULL, translated_size = NULL, last_mod_time = NULL \
                 WHERE local_relpath = 'd/secret.txt' AND op_depth = 0",
                [],
            )
            .expect("update");
        assert_eq!(n, 1);
        std::fs::remove_file(wc.join("d/secret.txt")).expect("rm");
    };
    let (theirs, ours) = prepare(&repo, &sb, "excluded", 1, exclude).await;
    let before = nodes_snapshot(&ours);

    let (ok, err) = svn_update(&repo, &theirs, 3);
    assert!(!ok && err.contains("excluded by server"), "svn: {err}");
    let e = our_update(&repo, &ours, 3)
        .await
        .expect_err("the copy of a server-excluded node must be refused");
    assert!(
        e.to_string()
            .contains("Cannot copy 'd/secret.txt' excluded by server"),
        "unclear error: {e}"
    );

    // Nothing below `d` changed: the local edit and every row are as they were.
    let below_d = |rows: Vec<String>| -> Vec<String> {
        rows.into_iter().filter(|r| r.starts_with("d")).collect()
    };
    assert_eq!(
        below_d(nodes_snapshot(&ours)),
        below_d(before),
        "rows below d changed"
    );
    assert_eq!(
        below_d(nodes_snapshot(&ours)),
        below_d(nodes_snapshot(&theirs)),
        "rows below d differ from svn"
    );
    assert_eq!(read(&ours.join("d/x.txt")), "x\nedited\n");
    assert_eq!(
        tree_snapshot(&ours),
        tree_snapshot(&theirs),
        "working files differ"
    );
    // The root row is the one deliberate difference. svn has already committed the root as
    // `incomplete` at r3 when the copy fails; our editor commits wc.db in batches
    // (`update::batch`) and the failed batch rolls back whole, so the root stays at r1. Both
    // resume on the next update, which the last step checks.
    let not_root = |rows: Vec<String>| -> Vec<String> {
        rows.into_iter().filter(|r| !r.starts_with('|')).collect()
    };
    assert_eq!(
        not_root(nodes_snapshot(&ours)),
        not_root(nodes_snapshot(&theirs)),
        "NODES rows differ"
    );
    let ours_root = nodes_snapshot(&ours)[0].clone();
    assert!(
        ours_root.starts_with("|0|normal|dir|Some(1)|")
            || ours_root.starts_with("|0|incomplete|dir|Some(3)|"),
        "root row: {ours_root}"
    );
    let status_without_root = |wc: &Path| -> String {
        svn_status(&oracle, wc, &[])
            .lines()
            .filter(|l| !l.ends_with(" ."))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        status_without_root(&ours),
        status_without_root(&theirs),
        "svn status differs"
    );

    // Once the user gives up the local edit, nothing needs a copy: both updates finish and
    // delete `d`, the excluded node with it.
    for wc in [&theirs, &ours] {
        write(&wc.join("d/x.txt"), "x\n");
    }
    let (ok, err) = svn_update(&repo, &theirs, 3);
    assert!(ok, "svn update after the revert failed: {err}");
    our_update(&repo, &ours, 3)
        .await
        .expect("update after the revert");
    assert_same(&oracle, &theirs, &ours, "after the revert");
}
