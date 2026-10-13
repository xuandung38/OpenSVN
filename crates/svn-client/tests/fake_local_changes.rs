//! Updates over local modifications: merge, text/property/tree conflicts.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use common::*;
use svn_client::{UpdateAction, checkout, update};
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth};
use svn_wc::conflicts::{Conflict, ConflictSkel, TreeAction, TreeReason};

async fn wc_with(
    files: &[svn_ra_api::fake::FakeChange],
) -> (FakeRaSession, Sandbox, std::path::PathBuf) {
    let s = session();
    s.seed("a", "r1", files).expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &s,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    (s, sb, wc)
}

async fn up(s: &FakeRaSession, wc: &std::path::Path) -> svn_client::UpdateSummary {
    update(&ctx(), s, wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update")
}

fn conflict_of(db: &svn_wc::db::WcDb, path: &str) -> ConflictSkel {
    let rec = db
        .conflict(&rp(path))
        .expect("read")
        .expect("conflict recorded");
    ConflictSkel::from_skel(&rec.skel).expect("skel")
}

#[tokio::test(flavor = "multi_thread")]
async fn disjoint_local_and_server_edits_merge_cleanly() {
    let (s, _sb, wc) = wc_with(&[put("f.txt", "l1\nl2\nl3\nl4\nl5\nl6\nl7\n")]).await;
    std::fs::write(wc.join("f.txt"), "L1\nl2\nl3\nl4\nl5\nl6\nl7\n").expect("edit");
    s.seed("a", "r2", &[put("f.txt", "l1\nl2\nl3\nl4\nl5\nl6\nL7\n")])
        .expect("seed");
    let sum = up(&s, &wc).await;
    assert_eq!(read(&wc.join("f.txt")), "L1\nl2\nl3\nl4\nl5\nl6\nL7\n");
    assert!(sum.events.iter().any(|e| e.action == UpdateAction::Merged));
    let db = open_db(&wc);
    assert!(db.conflict(&rp("f.txt")).expect("read").is_none());
    assert!(db.work_items().expect("queue").is_empty());
    let node = db.read_base(&rp("f.txt")).expect("read").expect("node");
    assert_eq!(node.revision, Some(2));
    // The pristine is the server text, so the local change is still a local change.
    assert!(svn_client::wc::inspect::text_modified(&db, &rp("f.txt"), &node).expect("modified"));
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_edits_leave_markers_and_a_text_conflict() {
    let (s, _sb, wc) = wc_with(&[put("f.txt", "a\nb\nc\n")]).await;
    std::fs::write(wc.join("f.txt"), "a\nmine\nc\n").expect("edit");
    s.seed("a", "r2", &[put("f.txt", "a\ntheirs\nc\n")])
        .expect("seed");
    let sum = up(&s, &wc).await;
    assert!(
        sum.events
            .iter()
            .any(|e| e.action == UpdateAction::Conflicted)
    );
    let merged = read(&wc.join("f.txt"));
    assert!(merged.contains("<<<<<<< .mine\nmine\n"), "{merged}");
    assert!(merged.contains("||||||| .r1\nb\n"), "{merged}");
    assert!(merged.contains("=======\ntheirs\n>>>>>>> .r2"), "{merged}");
    assert_eq!(read(&wc.join("f.txt.mine")), "a\nmine\nc\n");
    assert_eq!(read(&wc.join("f.txt.r1")), "a\nb\nc\n");
    assert_eq!(read(&wc.join("f.txt.r2")), "a\ntheirs\nc\n");
    let db = open_db(&wc);
    let skel = conflict_of(&db, "f.txt");
    let text = skel.text().expect("text conflict");
    assert_eq!(text.mine.as_deref(), Some("f.txt.mine"));
    assert_eq!(text.original.as_deref(), Some("f.txt.r1"));
    assert_eq!(text.their.as_deref(), Some("f.txt.r2"));
    assert!(db.work_items().expect("queue").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_file_conflict_keeps_the_local_file() {
    let (s, _sb, wc) = wc_with(&[
        put("p.bin", "base"),
        prop("p.bin", "svn:mime-type", Some("application/octet-stream")),
    ])
    .await;
    std::fs::write(wc.join("p.bin"), "mine").expect("edit");
    s.seed("a", "r2", &[put("p.bin", "theirs")]).expect("seed");
    up(&s, &wc).await;
    assert_eq!(read(&wc.join("p.bin")), "mine");
    assert_eq!(read(&wc.join("p.bin.r2")), "theirs");
    assert!(
        !wc.join("p.bin.mine").exists(),
        "the working file is the user's version"
    );
    assert!(conflict_of(&open_db(&wc), "p.bin").text().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn colliding_property_changes_record_a_property_conflict() {
    let (s, _sb, wc) = wc_with(&[put("f.txt", "x\n"), prop("f.txt", "color", Some("red"))]).await;
    let db = open_db(&wc);
    db.set_props(
        &rp("f.txt"),
        &vec![(b"color".to_vec(), b"green".to_vec())],
        &[],
    )
    .expect("local propset");
    drop(db);
    s.seed(
        "a",
        "r2",
        &[
            prop("f.txt", "color", Some("blue")),
            prop("f.txt", "size", Some("9")),
        ],
    )
    .expect("seed");
    up(&s, &wc).await;
    let db = open_db(&wc);
    let skel = conflict_of(&db, "f.txt");
    let p = skel.prop().expect("prop conflict");
    assert_eq!(p.names, vec!["color".to_owned()]);
    assert!(wc.join("f.txt.prej").is_file());
    assert!(read(&wc.join("f.txt.prej")).contains("color"));
    let actual = db.actual_props(&rp("f.txt")).expect("props");
    assert!(actual.contains(&(b"color".to_vec(), b"green".to_vec())));
    assert!(actual.contains(&(b"size".to_vec(), b"9".to_vec())));
    let pristine = db.pristine_props(&rp("f.txt")).expect("props");
    assert!(pristine.contains(&(b"color".to_vec(), b"blue".to_vec())));
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_an_edited_file_is_a_tree_conflict_and_keeps_data() {
    let (s, _sb, wc) = wc_with(&[put("f.txt", "x\n"), put("keep.txt", "k\n")]).await;
    std::fs::write(wc.join("f.txt"), "edited\n").expect("edit");
    s.seed("a", "r2", &[del("f.txt"), put("keep.txt", "k2\n")])
        .expect("seed");
    let sum = up(&s, &wc).await;
    assert_eq!(read(&wc.join("f.txt")), "edited\n");
    assert_eq!(read(&wc.join("keep.txt")), "k2\n");
    assert!(
        sum.events
            .iter()
            .any(|e| e.action == UpdateAction::TreeConflicted)
    );
    let db = open_db(&wc);
    let skel = conflict_of(&db, "f.txt");
    let tree = skel.tree().expect("tree conflict");
    assert_eq!(tree.reason, TreeReason::Edited);
    assert_eq!(tree.action, TreeAction::Delete);
    // Like svn: the node is no longer in BASE, it stays as a local copy of revision 1.
    assert!(db.read_base(&rp("f.txt")).expect("read").is_none());
    let cur = db.read_node(&rp("f.txt")).expect("read").expect("node");
    assert_eq!((cur.op_depth, cur.revision), (1, Some(1)));
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_edit_of_a_locally_deleted_file_is_a_tree_conflict() {
    let (s, _sb, wc) = wc_with(&[put("f.txt", "x\n")]).await;
    let db = open_db(&wc);
    db.op_delete(&rp("f.txt"), &[]).expect("schedule delete");
    std::fs::remove_file(wc.join("f.txt")).expect("rm");
    drop(db);
    s.seed("a", "r2", &[put("f.txt", "y\n")]).expect("seed");
    up(&s, &wc).await;
    assert!(
        !wc.join("f.txt").exists(),
        "the local delete must stay in effect"
    );
    let db = open_db(&wc);
    let tree = conflict_of(&db, "f.txt");
    let t = tree.tree().expect("tree conflict");
    assert_eq!(t.reason, TreeReason::Deleted);
    assert_eq!(t.action, TreeAction::Edit);
    assert!(matches!(tree.conflicts[0], Conflict::Tree(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_add_over_an_unversioned_file_is_a_tree_conflict_unless_checkout() {
    let (s, _sb, wc) = wc_with(&[put("a.txt", "a\n")]).await;
    std::fs::write(wc.join("new.txt"), "local\n").expect("write");
    s.seed("a", "r2", &[put("new.txt", "server\n")])
        .expect("seed");
    up(&s, &wc).await;
    assert_eq!(read(&wc.join("new.txt")), "local\n");
    let db = open_db(&wc);
    assert_eq!(
        conflict_of(&db, "new.txt").tree().expect("tree").reason,
        TreeReason::Unversioned
    );
    // BASE has the server's file; the working layer says "deleted", so the local file is not part of it.
    assert_eq!(
        db.read_base(&rp("new.txt"))
            .expect("read")
            .expect("node")
            .revision,
        Some(2)
    );
    assert_eq!(
        db.read_node(&rp("new.txt"))
            .expect("read")
            .expect("node")
            .presence,
        svn_wc::db::Presence::BaseDeleted
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_adopts_unversioned_files_in_the_way() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[put("new.txt", "server\n"), put("o.txt", "o\n")],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    std::fs::create_dir_all(&wc).expect("mkdir");
    std::fs::write(wc.join("new.txt"), "local\n").expect("write");
    let sum = checkout(
        &ctx(),
        &s,
        &wc,
        None,
        Depth::Infinity,
        true,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    assert_eq!(read(&wc.join("new.txt")), "local\n");
    assert!(sum.events.iter().any(|e| e.action == UpdateAction::Existed));
    let db = open_db(&wc);
    let node = db.read_base(&rp("new.txt")).expect("read").expect("node");
    assert!(svn_client::wc::inspect::text_modified(&db, &rp("new.txt"), &node).expect("modified"));
}
