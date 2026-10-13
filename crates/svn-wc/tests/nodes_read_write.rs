//! NODES read/write API on engine-created working copies (no svn needed) plus reading of
//! svn-created ones.
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use schema_support::*;
use svn_types::{Depth, ErrorCode, NodeKind, RelPath};
use svn_wc::db::{
    BaseNode, CommitInfo, ConflictRecord, ConflictTexts, NewWc, Presence, WcDb, find_wcroot,
    sql_err,
};
use svn_wc::skel::Skel;

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

fn new_wc(dir: &std::path::Path) -> WcDb {
    WcDb::create(
        dir,
        &NewWc {
            repos_root_url: "http://example.com/repo".into(),
            repos_uuid: "uuid-1".into(),
            repos_relpath: "trunk".into(),
            revision: 5,
            depth: Depth::Infinity,
        },
    )
    .expect("create")
}

fn base(kind: NodeKind, repos_relpath: &str, checksum: Option<&str>) -> BaseNode {
    BaseNode {
        kind,
        presence: Presence::Normal,
        repos_root_url: "http://example.com/repo".into(),
        repos_uuid: "uuid-1".into(),
        repos_relpath: repos_relpath.into(),
        revision: 5,
        changed_rev: Some(4),
        changed_date: Some(1_700_000_000_000_000),
        changed_author: Some("alice".into()),
        depth: (kind == NodeKind::Dir).then_some(Depth::Infinity),
        properties: Some(vec![(b"k".to_vec(), b"v".to_vec())]),
        checksum: checksum.map(str::to_owned),
        symlink_target: None,
        translated_size: None,
        last_mod_time: None,
        inherited_props: None,
        dav_cache: None,
    }
}

fn add_pristine(db: &WcDb, checksum: &str) {
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO pristine (checksum, size, refcount, md5_checksum) VALUES (?1, 3, 0, '$md5 $x')",
            [checksum],
        )
        .map_err(sql_err)?;
        Ok(())
    })
    .expect("pristine");
}

fn refcount(db: &WcDb, checksum: &str) -> i64 {
    db.with_read(|c| {
        c.query_row(
            "SELECT refcount FROM pristine WHERE checksum = ?1",
            [checksum],
            |r| r.get(0),
        )
        .map_err(sql_err)
    })
    .expect("refcount")
}

#[test]
fn created_wc_has_incomplete_root_and_base_add_roundtrips() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    let root = db.read_node(&RelPath::root()).expect("read").expect("root");
    assert_eq!(
        (root.presence, root.kind, root.op_depth),
        (Presence::Incomplete, NodeKind::Dir, 0)
    );
    assert_eq!(root.revision, Some(5));
    assert_eq!(root.repos_relpath.as_deref(), Some("trunk"));

    add_pristine(&db, "$sha1$aa");
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", Some("$sha1$aa")),
        &[],
    )
    .expect("add");
    db.base_add(&rp("sub"), &base(NodeKind::Dir, "trunk/sub", None), &[])
        .expect("add");
    db.base_add(
        &rp("sub/b"),
        &base(NodeKind::File, "trunk/sub/b", None),
        &[],
    )
    .expect("add");
    assert_eq!(refcount(&db, "$sha1$aa"), 1);

    let n = db.read_node(&rp("a.txt")).expect("read").expect("a");
    assert_eq!(n.checksum.as_deref(), Some("$sha1$aa"));
    assert_eq!(n.changed_author.as_deref(), Some("alice"));
    assert_eq!(n.properties, Some(vec![(b"k".to_vec(), b"v".to_vec())]));
    assert_eq!(db.read_base(&rp("a.txt")).expect("base"), Some(n));
    assert_eq!(
        db.children(&RelPath::root()).expect("kids"),
        vec![rp("a.txt"), rp("sub")]
    );
    assert_eq!(db.children(&rp("sub")).expect("kids"), vec![rp("sub/b")]);
    assert_eq!(db.read_node(&rp("missing")).expect("read"), None);
    assert_eq!(db.pristine_props(&rp("a.txt")).expect("props").len(), 1);

    db.set_depth(&rp("sub"), Depth::Files).expect("depth");
    assert_eq!(
        db.read_node(&rp("sub")).expect("r").expect("n").depth,
        Some(Depth::Files)
    );
    assert!(
        db.set_depth(&rp("a.txt"), Depth::Files).is_err(),
        "files have no depth"
    );
    db.record_fileinfo(&rp("a.txt"), Some(3), Some(42))
        .expect("fileinfo");
    let n = db.read_node(&rp("a.txt")).expect("r").expect("n");
    assert_eq!((n.translated_size, n.last_mod_time), (Some(3), Some(42)));

    // Removing BASE drops the subtree and decrements the pristine refcount via the trigger.
    db.base_remove(&rp("a.txt"), &[]).expect("remove");
    db.base_remove(&rp("sub"), &[]).expect("remove");
    assert_eq!(
        db.children(&RelPath::root()).expect("kids"),
        Vec::<RelPath>::new()
    );
    assert_eq!(refcount(&db, "$sha1$aa"), 0);
}

#[test]
fn working_ops_and_work_items_commit_in_one_transaction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", None),
        &[],
    )
    .expect("add");
    let item = Skel::parse(b"(file-remove a.txt)").expect("skel");
    db.op_delete(&rp("a.txt"), std::slice::from_ref(&item))
        .expect("delete");
    let cur = db.read_node(&rp("a.txt")).expect("r").expect("n");
    assert_eq!((cur.presence, cur.op_depth), (Presence::BaseDeleted, 1));
    let items = db.work_items().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].1, item);

    // A failing operation queues nothing and changes nothing.
    let err = db
        .op_delete(&rp("a.txt"), std::slice::from_ref(&item))
        .expect_err("already deleted");
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert_eq!(db.work_items().expect("items").len(), 1);
    db.complete_work_item(items[0].0).expect("done");
    assert!(db.work_items().expect("items").is_empty());
}

#[test]
fn operations_validate_their_preconditions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", None),
        &[],
    )
    .expect("add");
    assert!(
        db.op_add(&rp("a.txt"), NodeKind::File, None, &[]).is_err(),
        "already versioned"
    );
    assert!(
        db.op_add(&rp("nodir/x"), NodeKind::File, None, &[])
            .is_err(),
        "no parent"
    );
    assert!(
        db.op_add(&rp("x"), NodeKind::None, None, &[]).is_err(),
        "bad kind"
    );
    assert!(db.op_delete(&RelPath::root(), &[]).is_err(), "root");
    assert!(db.op_delete(&rp("nope"), &[]).is_err());
    assert!(db.op_copy(&rp("a.txt"), &rp("a.txt/inner"), &[]).is_err());
    assert!(db.op_copy(&rp("a.txt"), &rp("a.txt"), &[]).is_err());
    db.op_add(&rp("n.txt"), NodeKind::File, None, &[])
        .expect("add");
    let err = db
        .op_copy(&rp("n.txt"), &rp("m.txt"), &[])
        .expect_err("copy of add");
    assert!(err.message.contains("not supported"), "{}", err.message);
}

#[test]
fn props_changelist_and_conflict_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", None),
        &[],
    )
    .expect("add");
    let p = rp("a.txt");
    assert_eq!(
        db.actual_props(&p).expect("p"),
        db.pristine_props(&p).expect("p")
    );
    db.set_props(&p, &vec![(b"k".to_vec(), b"other".to_vec())], &[])
        .expect("set");
    assert_eq!(
        db.actual_props(&p).expect("p"),
        vec![(b"k".to_vec(), b"other".to_vec())]
    );
    assert_eq!(
        db.pristine_props(&p).expect("p"),
        vec![(b"k".to_vec(), b"v".to_vec())]
    );

    db.set_changelist(&p, Some("work")).expect("cl");
    assert_eq!(db.changelist(&p).expect("cl").as_deref(), Some("work"));
    assert!(db.set_changelist(&rp("ghost"), Some("x")).is_err());

    let record = ConflictRecord {
        skel: Skel::parse(b"((conflict-kinds) (text))").expect("skel"),
        texts: ConflictTexts::default(),
    };
    db.mark_conflict(&p, &record, &[]).expect("mark");
    assert_eq!(db.conflict(&p).expect("c"), Some(record));
    db.resolve(&p, &[]).expect("resolve");
    assert_eq!(db.conflict(&p).expect("c"), None);
    // props and changelist survive resolve; clearing them empties the row.
    assert_eq!(db.changelist(&p).expect("cl").as_deref(), Some("work"));
    db.set_changelist(&p, None).expect("cl");
    db.set_props(&p, &db.pristine_props(&p).expect("p"), &[])
        .expect("reset");
    let rows: i64 = db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM actual_node", [], |r| r.get(0))
                .map_err(sql_err)
        })
        .expect("count");
    assert_eq!(rows, 0);
}

#[test]
fn relpath_depth_function_matches_svn_definition() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    let got: Vec<Option<i64>> = db
        .with_read(|c| {
            ["''", "'a'", "'a/b'", "'a/b/c'", "NULL", "5"]
                .iter()
                .map(|v| {
                    c.query_row(&format!("SELECT relpath_depth({v})"), [], |r| r.get(0))
                        .map_err(sql_err)
                })
                .collect()
        })
        .expect("depth");
    assert_eq!(got, vec![Some(0), Some(1), Some(2), Some(3), None, None]);
}

#[test]
fn commit_of_modified_base_node_updates_revision_and_clears_local_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", None),
        &[],
    )
    .expect("add");
    let p = rp("a.txt");
    db.set_props(&p, &vec![(b"k".to_vec(), b"new".to_vec())], &[])
        .expect("set");
    db.set_changelist(&p, Some("cl")).expect("cl");
    let info = CommitInfo {
        new_rev: 9,
        date: Some(7),
        author: Some("bob".into()),
        ..CommitInfo::default()
    };
    db.commit_node(&p, &info, &[]).expect("commit");
    let n = db.read_node(&p).expect("r").expect("n");
    assert_eq!(
        (n.revision, n.changed_rev, n.changed_author.as_deref()),
        (Some(9), Some(9), Some("bob"))
    );
    assert_eq!(n.properties, Some(vec![(b"k".to_vec(), b"new".to_vec())]));
    assert_eq!(db.changelist(&p).expect("cl"), None);
    assert!(db.commit_node(&rp("ghost"), &info, &[]).is_err());
}

#[test]
fn reads_an_svn_created_working_copy() {
    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base_dir = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base_dir);
    let seed = checkout(&base_dir, &url, "seed", &[]);
    std::fs::create_dir(seed.join("d")).expect("mkdir");
    std::fs::write(seed.join("d/f"), "x\n").expect("write");
    svn(&seed, &["add", "-q", "d"]);
    svn(&seed, &["propset", "-q", "k", "v", "d/f"]);
    svn(&seed, &["commit", "-q", "-m", "s"]);
    let wc = checkout(&base_dir, &url, "wc", &[]);
    svn(&wc, &["propset", "-q", "k", "local", "d/f"]);
    svn(&wc, &["changelist", "-q", "cl", "d/f"]);
    svn(&wc, &["rm", "-q", "--force", "d/f"]);

    let db = WcDb::open(&find_wcroot(&wc.join("d/f")).expect("root")).expect("open");
    let d = db.read_node(&rp("d")).expect("r").expect("d");
    assert_eq!(
        (d.kind, d.presence, d.depth),
        (NodeKind::Dir, Presence::Normal, Some(Depth::Infinity))
    );
    let f = db.read_node(&rp("d/f")).expect("r").expect("f");
    assert_eq!(
        (f.presence, f.op_depth, f.kind),
        (Presence::BaseDeleted, 2, NodeKind::File)
    );
    let base = db.read_base(&rp("d/f")).expect("r").expect("f");
    assert!(
        base.checksum
            .as_deref()
            .is_some_and(|c| c.starts_with("$sha1$"))
    );
    assert_eq!(base.properties, Some(vec![(b"k".to_vec(), b"v".to_vec())]));
    assert_eq!(
        db.changelist(&rp("d/f")).expect("cl").as_deref(),
        Some("cl")
    );
    assert_eq!(db.children(&rp("d")).expect("kids"), vec![rp("d/f")]);
}

#[test]
fn empty_properties_blob_reads_as_no_properties() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", None),
        &[],
    )
    .expect("add");
    db.with_write(|tx| {
        tx.execute(
            "UPDATE nodes SET properties = X'' WHERE local_relpath = 'a.txt'",
            [],
        )
        .map_err(sql_err)?;
        Ok(())
    })
    .expect("blank props");
    let n = db.read_node(&rp("a.txt")).expect("read").expect("node");
    assert_eq!(n.properties, Some(Vec::new()));
}

#[test]
fn commit_batch_is_all_or_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    db.base_add(
        &rp("a.txt"),
        &base(NodeKind::File, "trunk/a.txt", None),
        &[],
    )
    .expect("add");
    let info = CommitInfo {
        new_rev: 9,
        date: Some(1),
        author: Some("bob".into()),
        ..CommitInfo::default()
    };
    let err = db
        .commit_nodes(
            &[(rp("a.txt"), info.clone()), (rp("ghost"), info.clone())],
            &[],
        )
        .expect_err("second path does not exist");
    assert_eq!(err.code, ErrorCode::PathNotFound);
    let n = db.read_node(&rp("a.txt")).expect("read").expect("node");
    assert_eq!(
        n.revision,
        Some(5),
        "first path must be rolled back with the batch"
    );
    db.commit_nodes(&[(rp("a.txt"), info)], &[]).expect("batch");
    assert_eq!(
        db.read_node(&rp("a.txt"))
            .expect("read")
            .expect("node")
            .revision,
        Some(9)
    );
}
