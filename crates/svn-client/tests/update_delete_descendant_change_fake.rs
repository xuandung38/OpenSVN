//! An incoming delete of a directory whose only local change is a delete or a move of something
//! below it (#159), over a fake repository: like svn, the directory stays as a local copy with
//! the tree conflict "local edit, incoming delete", and the user's delete or move is kept.
//! `update_delete_keep_copy_vs_svn` compares the same cases with svn 1.15.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};

use common::*;
use svn_client::local::{self, DeleteOptions};
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};
use svn_wc::conflicts::{Conflict, ConflictSkel, TreeAction, TreeReason};
use svn_wc::db::{Presence, WcDb};

/// r1: `d` (two files, a subdirectory) and `keep`; r2: `d` deleted.
fn repo() -> svn_ra_api::fake::FakeRaSession {
    let s = session();
    s.seed(
        "me",
        "r1",
        &[
            dir("d"),
            put("d/x.txt", "x\n"),
            put("d/y.txt", "y\n"),
            dir("d/sub"),
            put("d/sub/s.txt", "s\n"),
            dir("keep"),
            put("keep/k.txt", "k\n"),
        ],
    )
    .expect("r1");
    s.seed("me", "r2", &[del("d")]).expect("r2");
    s
}

async fn checkout_r1(sb: &Sandbox, name: &str) -> PathBuf {
    let wc = sb.path(name);
    checkout(
        &ctx(),
        &repo(),
        &wc,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    wc
}

async fn update_r2(wc: &Path) {
    update(
        &ctx(),
        &repo(),
        wc,
        Some(Revnum(2)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update");
}

/// `d` is a local copy (op depth 1) with the conflict "local edit, incoming delete".
fn assert_kept_with_tree_conflict(db: &WcDb, what: &str) {
    let d = db.read_node(&rp("d")).expect("read").expect("d");
    assert_eq!((d.op_depth, d.presence), (1, Presence::Normal), "{what}");
    assert!(db.read_base(&rp("d")).expect("read").is_none(), "{what}");
    let rec = db.conflict(&rp("d")).expect("read").expect("no conflict");
    let skel = ConflictSkel::from_skel(&rec.skel).expect("parse");
    assert!(
        skel.conflicts.iter().any(|c| matches!(
            c,
            Conflict::Tree(t) if t.reason == TreeReason::Edited && t.action == TreeAction::Delete
        )),
        "{what}: {:?}",
        skel.conflicts
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_keeps_a_directory_with_a_deleted_descendant() {
    let sb = Sandbox::new();
    let wc = checkout_r1(&sb, "rm").await;
    let db = open_db(&wc);
    db.set_lock_owner(ctx().lock_owner.clone());
    local::delete(
        &db,
        &CancelToken::new(),
        &[wc.join("d/x.txt")],
        &DeleteOptions::default(),
    )
    .expect("rm");
    drop(db);
    update_r2(&wc).await;

    let db = open_db(&wc);
    assert_kept_with_tree_conflict(&db, "rm");
    let x = db.read_node(&rp("d/x.txt")).expect("read").expect("x");
    assert_eq!((x.op_depth, x.presence), (2, Presence::BaseDeleted));
    assert!(!wc.join("d/x.txt").exists());
    assert_eq!(read(&wc.join("d/y.txt")), "y\n");
    assert_eq!(read(&wc.join("d/sub/s.txt")), "s\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_keeps_a_directory_with_a_moved_away_descendant() {
    let sb = Sandbox::new();
    let wc = checkout_r1(&sb, "mv").await;
    let db = open_db(&wc);
    db.set_lock_owner(ctx().lock_owner.clone());
    local::move_path(
        &db,
        &CancelToken::new(),
        &wc.join("d/sub/s.txt"),
        &wc.join("keep/s.txt"),
    )
    .expect("mv");
    drop(db);
    update_r2(&wc).await;

    let db = open_db(&wc);
    assert_kept_with_tree_conflict(&db, "mv");
    assert_eq!(
        db.moved_to(&rp("d/sub/s.txt")).expect("read"),
        Some(rp("keep/s.txt")),
        "the move is kept"
    );
    assert_eq!(read(&wc.join("keep/s.txt")), "s\n");
    assert_eq!(read(&wc.join("d/x.txt")), "x\n");
    assert!(!wc.join("d/sub/s.txt").exists());
}
