//! `WcDb` bindings of the store traits (`DbPristineIndex`, `DbWorkQueue`, `DbWorkingCopyOps`)
//! and the single-transaction writer `WcTxn`, exercised on real wc.db files.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::cell::Cell;

use svn_types::{Depth, ErrorCode, NodeKind, RelPath};
use svn_wc::conflicts::{
    Conflict, ConflictSkel, Operation, OperationInfo, Origin, PropConflict, TextConflict,
    TreeAction, TreeConflict, TreeReason,
};
use svn_wc::db::{BaseNode, DbWorkingCopyOps, NewWc, Presence, WcDb};
use svn_wc::pristine::{PristineIndex, Sha1Digest};
use svn_wc::workqueue::{WorkItem, WorkingCopyOps};

const ROOT: &str = "http://h/r";

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

fn new_wc() -> (tempfile::TempDir, WcDb) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = WcDb::create(
        tmp.path(),
        &NewWc {
            repos_root_url: ROOT.into(),
            repos_uuid: "uuid-1".into(),
            repos_relpath: String::new(),
            revision: 0,
            depth: Depth::Infinity,
        },
    )
    .expect("create");
    (tmp, db)
}

fn base_file(path: &str, rev: u64, checksum: Option<String>) -> BaseNode {
    BaseNode {
        kind: NodeKind::File,
        presence: Presence::Normal,
        repos_root_url: ROOT.into(),
        repos_uuid: "uuid-1".into(),
        repos_relpath: path.into(),
        revision: rev,
        changed_rev: Some(rev),
        changed_date: Some(1_000_000),
        changed_author: Some("me".into()),
        depth: None,
        properties: Some(Vec::new()),
        checksum,
        symlink_target: None,
        translated_size: None,
        last_mod_time: None,
        inherited_props: None,
        dav_cache: None,
    }
}

fn install_text(db: &WcDb, text: &[u8]) -> Sha1Digest {
    let mut store = db.pristine_store();
    store
        .install(&mut &text[..], None, None)
        .expect("install")
        .sha1
}

fn update_info() -> OperationInfo {
    OperationInfo {
        operation: Operation::Update,
        left: Origin::Unset,
        right: Origin::Unset,
    }
}

#[test]
fn pristine_index_records_looks_up_and_forgets_rows() {
    let (_tmp, db) = new_wc();
    let sha = install_text(&db, b"hello\n");
    let store = db.pristine_store();
    let row = store.index().lookup(&sha).expect("lookup").expect("row");
    assert_eq!(row.size, 6);
    assert!(row.hydrated);
    assert!(store.verify(&sha).expect("verify"));

    // Referenced by a BASE node: refcount 1 (schema trigger), not removable.
    db.base_add(
        &rp("a.txt"),
        &base_file("a.txt", 1, Some(sha.to_svn_string())),
        &[],
    )
    .expect("base_add");
    let mut store = db.pristine_store();
    assert!(store.index().unreferenced().expect("unref").is_empty());
    assert!(
        !store
            .index_mut()
            .forget_if_unreferenced(&sha)
            .expect("forget")
    );

    db.base_remove(&rp("a.txt"), &[]).expect("base_remove");
    assert_eq!(store.index().unreferenced().expect("unref"), vec![sha]);
    assert_eq!(store.remove_unreferenced().expect("cleanup"), 1);
    assert!(store.index().lookup(&sha).expect("lookup").is_none());
}

#[test]
fn run_work_queue_installs_files_and_records_fileinfo() {
    let (tmp, db) = new_wc();
    let sha = install_text(&db, b"line\n");
    let a = rp("a.txt");
    let item = WorkItem::FileInstall {
        relpath: "a.txt".into(),
        use_commit_times: false,
        record_fileinfo: true,
        source: None,
    }
    .to_skel();
    db.with_txn(|t| {
        t.base_add(&a, &base_file("a.txt", 3, Some(sha.to_svn_string())))?;
        t.queue_work(std::slice::from_ref(&item))
    })
    .expect("txn");
    assert_eq!(db.work_items().expect("items").len(), 1);

    assert_eq!(db.run_work_queue(None).expect("run"), 1);
    assert_eq!(
        std::fs::read(tmp.path().join("a.txt")).expect("read"),
        b"line\n"
    );
    assert!(db.work_items().expect("items").is_empty());
    let node = db.read_node(&a).expect("read").expect("node");
    assert_eq!(node.translated_size, Some(5));
    assert!(node.last_mod_time.is_some());
    // Nothing left: a second run is a no-op.
    assert_eq!(db.run_work_queue(None).expect("rerun"), 0);
}

#[test]
fn run_work_queue_stops_when_cancelled_and_keeps_items() {
    let (_tmp, db) = new_wc();
    db.queue_work(&[WorkItem::DirInstall {
        relpath: "d".into(),
    }
    .to_skel()])
        .expect("queue");
    let err = db.run_work_queue(Some(&|| true)).expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert_eq!(db.work_items().expect("items").len(), 1);
}

#[test]
fn file_commit_needs_a_handler() {
    let (_tmp, db) = new_wc();
    let item = WorkItem::FileCommit {
        relpath: "a.txt".into(),
    }
    .to_skel();
    db.queue_work(std::slice::from_ref(&item)).expect("queue");
    let err = db.run_work_queue(None).expect_err("no handler");
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup);
    assert_eq!(
        db.work_items().expect("items").len(),
        1,
        "item stays queued"
    );

    let calls = Cell::new(0);
    let mut ops = DbWorkingCopyOps::new(&db).with_file_commit(|_, p| {
        assert_eq!(p.as_str(), "a.txt");
        calls.set(calls.get() + 1);
        Ok(())
    });
    assert_eq!(db.run_work_queue_with(&mut ops, None).expect("run"), 1);
    assert_eq!(calls.get(), 1);
    assert!(db.work_items().expect("items").is_empty());
}

#[test]
fn base_remove_keeps_a_not_present_placeholder_and_is_repeatable() {
    let (_tmp, db) = new_wc();
    let a = rp("a.txt");
    db.base_add(&a, &base_file("a.txt", 4, None), &[])
        .expect("base_add");
    let mut ops = DbWorkingCopyOps::new(&db);
    for _ in 0..2 {
        ops.base_remove("a.txt", Some(7)).expect("base_remove");
        let base = db.read_base(&a).expect("read").expect("placeholder");
        assert_eq!(base.presence, Presence::NotPresent);
        assert_eq!(base.revision, Some(7));
        assert_eq!(base.repos_relpath.as_deref(), Some("a.txt"));
    }
    ops.base_remove("a.txt", None).expect("plain remove");
    assert!(db.read_base(&a).expect("read").is_none());
}

#[test]
fn install_info_resolves_repository_location_of_added_nodes() {
    let (_tmp, db) = new_wc();
    db.op_add(&rp("new.txt"), NodeKind::File, None, &[])
        .expect("add");
    let ops = DbWorkingCopyOps::new(&db);
    let info = ops.install_info("new.txt").expect("info");
    assert!(info.is_added);
    assert_eq!(info.repos_root_url, ROOT);
    assert_eq!(info.repos_relpath, "new.txt");
    assert!(ops.install_info("missing.txt").is_err());
}

#[test]
fn legacy_prej_install_is_refused() {
    let (_tmp, db) = new_wc();
    let mut ops = DbWorkingCopyOps::new(&db);
    let legacy = svn_wc::skel::Skel::parse(b"(prop)").expect("skel");
    assert!(ops.write_prop_reject_file("a.txt", Some(&legacy)).is_err());
    // No conflict recorded: nothing to write.
    ops.write_prop_reject_file("a.txt", None).expect("noop");
}

#[test]
fn prop_reject_file_is_written_from_the_conflict_record() {
    let (tmp, db) = new_wc();
    let a = rp("a.txt");
    db.base_add(&a, &base_file("a.txt", 1, None), &[])
        .expect("base_add");
    let prop = PropConflict {
        markers: vec!["a.txt.prej".into()],
        names: vec!["p".into()],
        old_props: Some(vec![(b"p".to_vec(), b"old\n".to_vec())]),
        mine_props: Some(vec![(b"p".to_vec(), b"mine\n".to_vec())]),
        their_props: Some(vec![(b"p".to_vec(), b"theirs\n".to_vec())]),
    };
    db.with_txn(|t| t.add_conflicts(&a, &update_info(), [Conflict::Prop(prop)]))
        .expect("conflict");
    let mut ops = DbWorkingCopyOps::new(&db);
    ops.write_prop_reject_file("a.txt", None).expect("write");
    let text = std::fs::read_to_string(tmp.path().join("a.txt.prej")).expect("prej");
    assert!(text.contains("Trying to change property 'p'"), "{text}");
    assert!(text.contains("<<<<<<< (local property value)"), "{text}");
}

#[test]
fn txn_rolls_back_every_statement_on_error() {
    let (_tmp, db) = new_wc();
    let a = rp("a.txt");
    let err = db
        .with_txn(|t| {
            t.base_add(&a, &base_file("a.txt", 1, None))?;
            t.set_actual_props(&a, Some(&vec![(b"x".to_vec(), b"1".to_vec())]))?;
            t.add_conflicts(
                &a,
                &update_info(),
                [Conflict::Text(TextConflict::default())],
            )?;
            t.queue_work(&[WorkItem::FileRemove {
                relpath: "a.txt".into(),
            }
            .to_skel()])?;
            Err::<(), _>(svn_types::SvnError::new(ErrorCode::Internal, "abort"))
        })
        .expect_err("aborted");
    assert_eq!(err.message, "abort");
    assert!(db.read_node(&a).expect("read").is_none());
    assert!(db.conflict(&a).expect("conflict").is_none());
    assert!(db.work_items().expect("items").is_empty());
}

#[test]
fn add_conflicts_replaces_conflicts_of_the_same_kind() {
    let (_tmp, db) = new_wc();
    let a = rp("a.txt");
    db.base_add(&a, &base_file("a.txt", 1, None), &[])
        .expect("base_add");
    let text = |m: &str| {
        Conflict::Text(TextConflict {
            original: None,
            mine: Some(m.into()),
            their: None,
        })
    };
    let tree = Conflict::Tree(TreeConflict {
        reason: TreeReason::Edited,
        action: TreeAction::Delete,
        move_src_op_root: None,
        move_dst_op_root: None,
    });
    db.with_txn(|t| t.add_conflicts(&a, &update_info(), [text("one"), tree.clone()]))
        .expect("first");
    db.with_txn(|t| t.add_conflicts(&a, &update_info(), [text("two")]))
        .expect("second");
    let rec = db.conflict(&a).expect("read").expect("record");
    let skel = ConflictSkel::from_skel(&rec.skel).expect("parse");
    assert_eq!(skel.conflicts.len(), 2);
    assert!(skel.conflicts.contains(&tree));
    assert!(skel.conflicts.contains(&text("two")));
    assert!(!skel.conflicts.contains(&text("one")));
}

#[test]
fn txn_tree_writers_update_base_subtrees() {
    let (_tmp, db) = new_wc();
    for (p, rev) in [("d", 1), ("d/a.txt", 1), ("d/sub", 1), ("d/sub/b.txt", 2)] {
        let mut n = base_file(&format!("trunk/{p}"), rev, None);
        if !p.ends_with(".txt") {
            n.kind = NodeKind::Dir;
            n.depth = Some(Depth::Infinity);
        }
        db.base_add(&rp(p), &n, &[]).expect("base_add");
    }
    db.with_txn(|t| {
        t.mark_incomplete(&rp("d"), 5, None)?;
        t.relocate_subtree(&rp("d"), "trunk/d", "branches/x/d")?;
        t.bump_revisions(&rp("d"), 5, &[rp("d/sub")])
    })
    .expect("txn");
    let d = db.read_base(&rp("d")).expect("read").expect("d");
    assert_eq!(d.presence, Presence::Incomplete);
    assert_eq!(d.revision, Some(5));
    assert_eq!(d.repos_relpath.as_deref(), Some("branches/x/d"));
    let a = db.read_base(&rp("d/a.txt")).expect("read").expect("a");
    assert_eq!(
        (a.revision, a.repos_relpath.as_deref()),
        (Some(5), Some("branches/x/d/a.txt"))
    );
    let b = db.read_base(&rp("d/sub/b.txt")).expect("read").expect("b");
    assert_eq!(b.revision, Some(2), "skipped subtree keeps its revision");

    // Incoming delete over local changes: BASE becomes a local copy, then an obstruction marker.
    db.with_txn(|t| t.convert_base_to_copy(&rp("d/sub")))
        .expect("copy");
    assert!(db.read_base(&rp("d/sub/b.txt")).expect("read").is_none());
    let cur = db
        .read_node(&rp("d/sub/b.txt"))
        .expect("read")
        .expect("copy");
    assert_eq!(cur.op_depth, 2);
    db.with_txn(|t| t.mark_base_deleted(&rp("d/a.txt"), NodeKind::File))
        .expect("obstruction");
    let cur = db.read_node(&rp("d/a.txt")).expect("read").expect("row");
    assert_eq!(cur.presence, Presence::BaseDeleted);
}

#[test]
fn moved_from_finds_the_move_source() {
    let (_tmp, db) = new_wc();
    db.base_add(&rp("a.txt"), &base_file("a.txt", 1, None), &[])
        .expect("base_add");
    db.op_move(&rp("a.txt"), &rp("b.txt"), &[]).expect("move");
    assert_eq!(db.moved_from(&rp("b.txt")).expect("q"), Some(rp("a.txt")));
    assert_eq!(db.moved_from(&rp("a.txt")).expect("q"), None);
}
