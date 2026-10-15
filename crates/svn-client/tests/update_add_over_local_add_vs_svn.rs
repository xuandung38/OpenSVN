//! An incoming add that finds a local add, a move destination or an unversioned file in its
//! way (found by `update_local_edits_prop`).
//!
//! svn 1.15 still adds the server's node to BASE, shadowed: by the working node for a local add
//! or move destination (status `R`, tree conflict "local add|moved-here, incoming add"), by a
//! `base-deleted` layer for an unversioned file in the way of a directory (status `D`, tree
//! conflict "local unversioned, incoming add"). We used to record the tree conflict but skip
//! the node, leaving no BASE row: wc.db and `svn status` differed from svn, and a commit after
//! resolving would have tried to add a node the repository already has.
//!
//! The comparisons with svn are skipped when no svn 1.15 is installed (they fail instead when
//! `OPENSVN_REQUIRE_SVN=1`); the fake-session tests always run.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use svn_client::local::{self, AddConfig, AddOptions};
use svn_client::{UpdateAction, checkout, update};
use svn_types::{CancelToken, Depth, MemoryLockOwnership, Revnum};
use svn_wc::conflicts::{ConflictSkel, TreeAction, TreeReason};
use svn_wc::db::{LockOwner, Presence, WcDb};

/// r1: `A/{f,B/}`; both working copies at r1 get `local`, the seed commits `server` as r2, both
/// update to r2. Returns `(oracle, svn's copy, ours)` with the temporary directories leaked.
async fn scenario(
    local: impl Fn(&Repo, &Path),
    server: impl Fn(&Repo, &Path),
) -> Option<(opensvn_svn_oracle::Oracle, PathBuf, PathBuf)> {
    let oracle = oracle()?;
    let repo = Repo::new(&oracle);
    let seed = repo.checkout("seed");
    write(&seed.join("A/f"), "f\n");
    std::fs::create_dir_all(seed.join("A/B")).expect("mkdir");
    repo.svn(&seed, &["add", "-q", "A"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
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
    local(&repo, &theirs);
    local(&repo, &ours);
    server(&repo, &seed);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
    update(
        &ctx(),
        &repo.fake(None),
        &ours,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update");
    std::mem::forget(sb);
    std::mem::forget(repo);
    Some((oracle, theirs, ours))
}

fn status_line(o: &opensvn_svn_oracle::Oracle, wc: &Path, path: &str) -> String {
    svn_status(o, wc, &[])
        .lines()
        .find(|l| l.ends_with(&format!(" {path}")))
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_file_add_over_local_file_add_is_shadowed_like_svn() {
    let Some((o, t, w)) = scenario(
        |repo, wc| {
            write(&wc.join("A/n"), "mine\n");
            repo.svn(wc, &["add", "-q", "A/n"]);
        },
        |repo, seed| {
            write(&seed.join("A/n"), "theirs\n");
            repo.svn(seed, &["add", "-q", "A/n"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "file add over local file add");
    assert_eq!(status_line(&o, &w, "A/n"), "R     C A/n");
    assert_eq!(read(&w.join("A/n")), "mine\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_dir_add_over_local_dir_add_is_shadowed_like_svn() {
    let Some((o, t, w)) = scenario(
        |repo, wc| {
            write(&wc.join("A/d/x"), "my x\n");
            write(&wc.join("A/d/mine"), "only mine\n");
            repo.svn(wc, &["add", "-q", "A/d"]);
        },
        |repo, seed| {
            write(&seed.join("A/d/x"), "their x\n");
            write(&seed.join("A/d/sub/y"), "y\n");
            repo.svn(seed, &["add", "-q", "A/d"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "dir add over local dir add");
    assert_eq!(status_line(&o, &w, "A/d"), "R     C A/d");
    assert_eq!(read(&w.join("A/d/x")), "my x\n");
    assert!(
        !w.join("A/d/sub").exists(),
        "the incoming subtree is BASE only"
    );
}

/// The destination of a local move: the conflict reason is `moved-here`.
///
/// One known difference remains, from a missing feature rather than this fix: after an update
/// that does not touch the move's source, svn re-bases the destination on the new revision
/// (`bump_moved_away`), so the `op_depth > 0` rows of `n` carry r2 there and r1 here. Those
/// revisions are left out of the comparison.
#[tokio::test(flavor = "multi_thread")]
async fn incoming_file_add_over_move_destination_is_shadowed_like_svn() {
    let Some((o, t, w)) = scenario(
        |repo, wc| {
            repo.svn(wc, &["mv", "-q", "A/B", "n"]);
        },
        |repo, seed| {
            write(&seed.join("n"), "theirs\n");
            repo.svn(seed, &["add", "-q", "n"]);
        },
    )
    .await
    else {
        return;
    };
    assert_eq!(tree_snapshot(&w), tree_snapshot(&t), "working files differ");
    let without_moved_revision = |wc: &Path| -> Vec<String> {
        nodes_snapshot(wc)
            .into_iter()
            .map(|r| {
                let f: Vec<&str> = r.split('|').collect();
                if f[0] == "n" && f[1] != "0" {
                    format!(
                        "{}|{}|{}|{}|REV|{}",
                        f[0],
                        f[1],
                        f[2],
                        f[3],
                        f[5..].join("|")
                    )
                } else {
                    r
                }
            })
            .collect()
    };
    assert_eq!(
        without_moved_revision(&w),
        without_moved_revision(&t),
        "NODES rows differ"
    );
    assert_eq!(
        actual_snapshot(&w),
        actual_snapshot(&t),
        "ACTUAL rows differ"
    );
    assert_eq!(
        svn_status(&o, &w, &[]),
        svn_status(&o, &t, &[]),
        "svn status differs"
    );
    assert!(
        actual_snapshot(&w)
            .iter()
            .any(|r| r.starts_with("n|") && r.contains("moved-here added")),
        "{:?}",
        actual_snapshot(&w)
    );
}

/// The same update deletes the move's source first (svn's reporter sends the deletions of a
/// directory before its adds): the move is broken, so svn calls the destination `added`.
#[tokio::test(flavor = "multi_thread")]
async fn incoming_add_over_destination_of_a_broken_move_is_a_local_add_like_svn() {
    let Some((o, t, w)) = scenario(
        |repo, wc| {
            repo.svn(wc, &["mv", "-q", "A", "n"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "A"]);
            std::fs::create_dir(seed.join("n")).expect("mkdir");
            repo.svn(seed, &["add", "-q", "n"]);
        },
    )
    .await
    else {
        return;
    };
    // The conflict on the source `A` says `moved-away` in svn and `deleted` here: our update
    // does not track local moves yet. Everything else is compared.
    let source_reason = |rows: Vec<String>| -> Vec<String> {
        rows.into_iter()
            .map(|r| r.replace("moved-away deleted A n)", "deleted deleted)"))
            .collect()
    };
    let status = |wc: &Path| -> Vec<String> {
        let mut lines: Vec<String> = svn_status(&o, wc, &[])
            .lines()
            .map(|l| l.replace("local dir moved away", "local dir delete"))
            .collect();
        lines.sort();
        lines
    };
    assert_eq!(tree_snapshot(&w), tree_snapshot(&t), "working files differ");
    assert_eq!(nodes_snapshot(&w), nodes_snapshot(&t), "NODES rows differ");
    assert_eq!(
        source_reason(actual_snapshot(&w)),
        source_reason(actual_snapshot(&t)),
        "ACTUAL rows differ"
    );
    assert_eq!(status(&w), status(&t), "svn status differs");
    assert!(
        actual_snapshot(&w)
            .iter()
            .any(|r| r.starts_with("n|") && r.contains("(tree () added added)")),
        "{:?}",
        actual_snapshot(&w)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_dir_add_over_unversioned_file_is_shadowed_like_svn() {
    let Some((o, t, w)) = scenario(
        |_, wc| write(&wc.join("A/d"), "my file\n"),
        |repo, seed| {
            write(&seed.join("A/d/x"), "x\n");
            write(&seed.join("A/d/e/y"), "y\n");
            repo.svn(seed, &["add", "-q", "A/d"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "dir add over unversioned file");
    assert_eq!(status_line(&o, &w, "A/d"), "D     C A/d");
    assert_eq!(read(&w.join("A/d")), "my file\n");
}

// ---- Without svn: the fake session alone ----------------------------------------------------

fn open_locked(wc: &Path) -> WcDb {
    let db = open_db(wc);
    db.set_lock_owner(LockOwner {
        store: std::sync::Arc::new(MemoryLockOwnership::for_current_process("test-boot")),
        pid: std::process::id(),
        boot_id: "test-boot".to_owned(),
    });
    db
}

fn tree_conflict(db: &WcDb, path: &str) -> (TreeReason, TreeAction) {
    let rec = db.conflict(&rp(path)).expect("read").expect("conflict");
    let skel = ConflictSkel::from_skel(&rec.skel).expect("skel");
    let tree = skel.tree().expect("tree conflict");
    (tree.reason.clone(), tree.action.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_adds_over_local_adds_moves_and_files_keep_base_rows() {
    let s = session();
    s.seed("a", "r1", &[put("A/f", "f\n"), dir("A/B")])
        .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    {
        let db = open_locked(&wc);
        write(&wc.join("A/n"), "mine\n");
        write(&wc.join("A/d/x"), "my x\n");
        let targets = [wc.join("A/n"), wc.join("A/d")];
        local::add(
            &db,
            &cancel,
            &targets,
            &AddOptions::default(),
            &AddConfig::default(),
        )
        .expect("add");
        local::move_path(&db, &cancel, &wc.join("A/B"), &wc.join("m")).expect("mv");
    }
    write(&wc.join("u"), "unversioned\n");
    s.seed(
        "a",
        "r2",
        &[
            put("A/n", "theirs\n"),
            put("A/d/x", "their x\n"),
            put("m", "theirs\n"),
            put("u/x", "x\n"),
        ],
    )
    .expect("seed");
    let sum = update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert_eq!(sum.revision, Some(Revnum(2)));
    let db = open_db(&wc);
    for (path, reason) in [
        ("A/n", TreeReason::Added),
        ("A/d", TreeReason::Added),
        ("m", TreeReason::MovedHere),
        ("u", TreeReason::Unversioned),
    ] {
        let base = db.read_base(&rp(path)).expect("read").expect("BASE row");
        assert_eq!(
            (base.presence, base.revision),
            (Presence::Normal, Some(2)),
            "{path}"
        );
        assert_eq!(
            tree_conflict(&db, path),
            (reason, TreeAction::Add),
            "{path}"
        );
        assert!(
            sum.events
                .iter()
                .any(|e| e.path.as_str() == path && e.action == UpdateAction::TreeConflicted),
            "{path}: no tree-conflict notification"
        );
    }
    // Below the shadowed directories: BASE only, the disk keeps what the user had.
    assert!(db.read_base(&rp("A/d/x")).expect("read").is_some());
    assert!(db.read_base(&rp("u/x")).expect("read").is_some());
    assert_eq!(read(&wc.join("A/n")), "mine\n");
    assert_eq!(read(&wc.join("A/d/x")), "my x\n");
    assert_eq!(read(&wc.join("u")), "unversioned\n");
    assert!(wc.join("m").is_dir());
    assert!(db.work_items().expect("queue").is_empty());
}
