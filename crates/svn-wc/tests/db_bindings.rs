//! `WcDb` bindings of the store traits (`DbPristineIndex`, `DbWorkQueue`, `DbWorkingCopyOps`)
//! and the single-transaction writer `WcTxn`, exercised on real wc.db files.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::cell::Cell;

use svn_types::{Depth, ErrorCode, NodeKind, RelPath};
use svn_wc::conflicts::{
    Conflict, ConflictSkel, Operation, OperationInfo, Origin, PropConflict, TextConflict,
    TreeAction, TreeConflict, TreeReason,
};
use svn_wc::db::{BaseNode, CommitInfo, DbWorkingCopyOps, LockInfo, NewWc, Presence, WcDb};
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
    // `b.txt` is at r2 below `d/sub` at r1: like svn, a copy of its own at its own depth, with a
    // `not-present` row in the layer of `d/sub`.
    assert_eq!(cur.op_depth, 3);
    let sub = db.read_node(&rp("d/sub")).expect("read").expect("copy");
    assert_eq!((sub.op_depth, sub.presence), (2, Presence::Normal));
    let layer = db
        .with_read(|c| {
            Ok(c.query_row(
                "SELECT presence FROM nodes WHERE local_relpath = 'd/sub/b.txt' AND op_depth = 2",
                [],
                |r| r.get::<_, String>(0),
            )
            .expect("row"))
        })
        .expect("read");
    assert_eq!(layer, "not-present");
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

#[test]
fn txn_lock_cache_and_commit_node_join_one_transaction() {
    let (_tmp, db) = new_wc();
    let a = rp("a.txt");
    db.base_add(&a, &base_file("a.txt", 1, None), &[])
        .expect("base_add");
    let lock = LockInfo {
        token: "opaquelocktoken:1".into(),
        owner: Some("me".into()),
        comment: None,
        date: Some(5),
    };
    assert_eq!(
        db.with_txn(|t| t.lock_add(&rp("missing"), &lock))
            .expect_err("no BASE")
            .code,
        ErrorCode::PathNotFound
    );
    db.with_txn(|t| t.lock_add(&a, &lock)).expect("lock_add");
    assert_eq!(db.repos_lock(&a).expect("lock"), Some(lock.clone()));

    // Rolled back together: the lock survives and BASE keeps r1.
    let info = CommitInfo {
        new_rev: 2,
        ..CommitInfo::default()
    };
    db.with_txn(|t| {
        t.lock_remove(&a)?;
        t.commit_node(&a, &info)?;
        Err::<(), _>(svn_types::SvnError::new(ErrorCode::Internal, "abort"))
    })
    .expect_err("aborted");
    assert!(db.repos_lock(&a).expect("lock").is_some());
    assert_eq!(
        db.read_node(&a).expect("read").expect("node").revision,
        Some(1)
    );

    for _ in 0..2 {
        db.with_txn(|t| {
            t.lock_remove(&a)?;
            t.commit_node(&a, &info)
        })
        .expect("commit, repeatable");
    }
    assert!(db.repos_lock(&a).expect("lock").is_none());
    assert_eq!(
        db.read_node(&a).expect("read").expect("node").revision,
        Some(2)
    );
}

#[test]
fn relocation_to_a_path_below_the_old_one_is_applied_once() {
    let (_tmp, db) = new_wc();
    for (p, repos) in [
        ("d", "lib"),
        ("d/s.txt", "lib/sub/s.txt"),
        ("d/old.txt", "lib/old.txt"),
        ("d/sw.txt", "elsewhere/sw.txt"),
    ] {
        let mut n = base_file(repos, 1, None);
        if p == "d" {
            n.kind = NodeKind::Dir;
            n.depth = Some(Depth::Infinity);
        }
        db.base_add(&rp(p), &n, &[]).expect("base_add");
    }
    // The drive already moved `d` to `lib/sub` and wrote `d/s.txt` at its new path.
    db.with_txn(|t| t.mark_incomplete(&rp("d"), 2, Some("lib/sub")))
        .expect("mark");
    db.with_txn(|t| t.relocate_subtree(&rp("d"), "lib", "lib/sub"))
        .expect("relocate");
    let path = |p: &str| {
        db.read_base(&rp(p))
            .expect("read")
            .expect("node")
            .repos_relpath
            .expect("path")
    };
    assert_eq!(path("d"), "lib/sub");
    assert_eq!(path("d/s.txt"), "lib/sub/s.txt");
    assert_eq!(
        path("d/old.txt"),
        "lib/sub/old.txt",
        "untouched node follows"
    );
    assert_eq!(
        path("d/sw.txt"),
        "elsewhere/sw.txt",
        "switched node keeps its path"
    );
}

#[test]
fn batch_writers_for_commit_post_processing() {
    let (_tmp, db) = new_wc();
    db.base_add(&rp("a.txt"), &base_file("a.txt", 1, None), &[])
        .expect("base");
    db.base_add(&rp("b.txt"), &base_file("b.txt", 1, None), &[])
        .expect("base");

    // Only nodes with an ACTUAL_NODE row are listed.
    assert!(
        db.actual_rows_below(&RelPath::root())
            .expect("rows")
            .is_empty()
    );
    db.set_changelist(&rp("b.txt"), Some("cl")).expect("cl");
    assert_eq!(
        db.actual_rows_below(&RelPath::root()).expect("rows"),
        vec![rp("b.txt")]
    );
    assert!(db.actual_rows_below(&rp("a.txt")).expect("rows").is_empty());

    // Many reads in one read transaction; a nested write fails instead of escaping it.
    let n = db
        .read_txn(|| {
            let a = db.read_node(&rp("a.txt"))?.is_some();
            let b = db.changelist(&rp("b.txt"))?;
            assert!(db.queue_work(&[]).is_err());
            Ok(usize::from(a) + usize::from(b.is_some()))
        })
        .expect("read txn");
    assert_eq!(n, 2);
    // The connection is usable for writes again afterwards.
    db.queue_work(&[]).expect("write after read txn");

    // A staged pristine and the completion of work items join the caller's transaction.
    let batch = db.pristine_store().batch().expect("batch");
    let rec = batch
        .stage(&mut &b"hello\n"[..], None, None)
        .expect("stage");
    let items = [
        WorkItem::FileCommit {
            relpath: "a.txt".into(),
        }
        .to_skel(),
        WorkItem::FileCommit {
            relpath: "b.txt".into(),
        }
        .to_skel(),
    ];
    db.queue_work(&items).expect("queue");
    let ids: Vec<i64> = db
        .work_items()
        .expect("items")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids.len(), 2);
    let err = db.with_txn(|t| {
        t.record_pristine(&rec)?;
        t.complete_work_items(&ids)?;
        Err::<(), _>(svn_types::SvnError::new(ErrorCode::Internal, "roll back"))
    });
    assert!(err.is_err());
    assert_eq!(db.work_items().expect("items").len(), 2);
    assert!(
        db.pristine_store()
            .index()
            .lookup(&rec.sha1)
            .expect("lookup")
            .is_none()
    );
    db.with_txn(|t| {
        t.record_pristine(&rec)?;
        t.record_pristine(&rec)?;
        t.complete_work_items(&ids)?;
        t.complete_work_items(&ids)
    })
    .expect("commit");
    assert!(db.work_items().expect("items").is_empty());
    assert_eq!(
        db.pristine_store()
            .index()
            .lookup(&rec.sha1)
            .expect("lookup"),
        Some(rec)
    );
}
