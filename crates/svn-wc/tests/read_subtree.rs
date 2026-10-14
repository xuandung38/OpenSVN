//! `WcDb::read_subtree`: the bulk read used by status gives the rows the single-node readers
//! give, in path order, whether the whole-tree read is split over several connections (large
//! working copies) or not (subtrees).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use svn_types::{Depth, RelPath};
use svn_wc::WcDb;
use svn_wc::db::{NewWc, Node};

const DIRS: usize = 30;
const PER_DIR: usize = 200;

fn rel(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

/// A working copy whose NODES table holds more than 6000 rows inserted out of path order, with
/// working layers on some paths, ACTUAL rows and a cached lock.
fn build() -> (tempfile::TempDir, WcDb) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = WcDb::create(
        dir.path(),
        &NewWc {
            repos_root_url: "https://svn.example.invalid/repo".into(),
            repos_uuid: "00000000-0000-0000-0000-000000000000".into(),
            repos_relpath: "trunk".into(),
            revision: 1,
            depth: Depth::Infinity,
        },
    )
    .expect("create wc");
    let wc_id = db.wc_id();
    db.with_write(|tx| {
        let e = svn_wc::db::sql_err;
        let mut node = tx
            .prepare(
                "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
                 repos_path, revision, presence, depth, kind, changed_revision, changed_date, \
                 changed_author, translated_size, last_mod_time, properties) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, 0, 'me', ?11, ?12, ?13)",
            )
            .map_err(e)?;
        // Directories in reverse order, files interleaved across directories: rowid order is
        // not path order.
        for d in (0..DIRS).rev() {
            let p = format!("d{d:03}");
            node.execute(rusqlite::params![
                wc_id,
                &p,
                0,
                "",
                1,
                format!("trunk/{p}"),
                1,
                "normal",
                "infinity",
                "dir",
                None::<i64>,
                None::<i64>,
                Some(b"(4 name 5 value)".to_vec()),
            ])
            .map_err(e)?;
        }
        for f in 0..PER_DIR {
            for d in 0..DIRS {
                let parent = format!("d{d:03}");
                let p = format!("{parent}/f{f:04}.txt");
                node.execute(rusqlite::params![
                    wc_id,
                    &p,
                    0,
                    &parent,
                    1,
                    format!("trunk/{p}"),
                    1,
                    "normal",
                    None::<String>,
                    "file",
                    Some(i64::try_from(f).unwrap()),
                    Some(1_000_000 + i64::try_from(d).unwrap()),
                    None::<Vec<u8>>,
                ])
                .map_err(e)?;
                if f % 37 == 0 {
                    // Deleted, then added again above the delete.
                    for (depth, presence) in [(2, "base-deleted"), (3, "normal")] {
                        node.execute(rusqlite::params![
                            wc_id,
                            &p,
                            depth,
                            &parent,
                            None::<i64>,
                            None::<String>,
                            None::<i64>,
                            presence,
                            None::<String>,
                            "file",
                            None::<i64>,
                            None::<i64>,
                            None::<Vec<u8>>,
                        ])
                        .map_err(e)?;
                    }
                }
            }
        }
        tx.execute(
            "INSERT INTO actual_node (wc_id, local_relpath, parent_relpath, changelist, properties) \
             VALUES (?1, 'd005/f0003.txt', 'd005', 'work', NULL), \
                    (?1, 'd007', '', NULL, ?2)",
            rusqlite::params![wc_id, b"(1 a 1 b)".to_vec()],
        )
        .map_err(e)?;
        tx.execute(
            "INSERT INTO lock (repos_id, repos_relpath, lock_token, lock_owner) \
             VALUES (1, 'trunk/d001/f0001.txt', 'opaquelocktoken:x', 'me')",
            [],
        )
        .map_err(e)?;
        Ok(())
    })
    .expect("insert");
    (dir, db)
}

fn key(n: &Node) -> (&[u8], u32) {
    (n.relpath.as_str().as_bytes(), n.op_depth)
}

/// Rows of `rows` agree with the single-node readers and come in path, then layer, order.
fn assert_like_readers(db: &WcDb, rows: &[Node]) {
    assert!(rows.windows(2).all(|w| key(&w[0]) < key(&w[1])), "order");
    let mut i = 0;
    while i < rows.len() {
        let path = &rows[i].relpath;
        let run = rows[i..].iter().take_while(|n| n.relpath == *path).count();
        let layers = &rows[i..i + run];
        assert_eq!(db.read_node(path).expect("read").as_ref(), layers.last());
        assert_eq!(
            db.read_base(path).expect("base").as_ref(),
            layers.first().filter(|n| n.op_depth == 0)
        );
        i += run;
    }
}

#[test]
fn whole_tree_read_matches_the_node_readers() {
    let (_dir, db) = build();
    let all = db.read_subtree(&RelPath::root()).expect("read");
    let total: i64 = db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
                .map_err(svn_wc::db::sql_err)
        })
        .expect("count");
    assert!(total > 6000, "large enough to be split: {total}");
    assert_eq!(i64::try_from(all.nodes.len()).unwrap(), total);
    assert_like_readers(&db, &all.nodes);

    let actual: Vec<_> = all.actual.iter().map(|a| a.relpath.as_str()).collect();
    assert_eq!(actual, ["d005/f0003.txt", "d007"]);
    assert_eq!(all.actual[0].changelist.as_deref(), Some("work"));
    assert_eq!(
        db.actual_props(&rel("d007")).expect("props"),
        all.actual[1].properties.clone().expect("props")
    );
    assert_eq!(all.locks.len(), 1);
    assert_eq!(
        db.repos_lock(&rel("d001/f0001.txt"))
            .expect("lock")
            .as_ref(),
        Some(&all.locks[0].lock)
    );
}

#[test]
fn subtree_read_is_the_slice_of_the_whole_tree() {
    let (_dir, db) = build();
    let all = db.read_subtree(&RelPath::root()).expect("read");
    for sub in ["d000", "d017", "d029", "d017/f0037.txt"] {
        let root = rel(sub);
        let part = db.read_subtree(&root).expect("read");
        let expected: Vec<&Node> = all
            .nodes
            .iter()
            .filter(|n| n.relpath.starts_with(&root))
            .collect();
        assert_eq!(part.nodes.iter().collect::<Vec<_>>(), expected, "{sub}");
        assert_like_readers(&db, &part.nodes);
    }
}
