//! `WcDb::read_subtree` against a writer that wants to commit while the split read runs (#145).
//! The main connection holds the shared lock; a writer that commits then takes PENDING and waits
//! for it. A helper connection that waited for its own shared lock would deadlock with the
//! writer until the 10 s busy timeout and fail with `WcBusy`. The helper must give its part back
//! to the main connection instead.
//! Run with `cargo test -p svn-wc --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::{Duration, Instant};

use svn_types::{Depth, RelPath};
use svn_wc::WcDb;
use svn_wc::db::NewWc;

/// A working copy with enough NODES rows for the whole-tree read to be split.
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
                 repos_path, revision, presence, kind, changed_revision, changed_date, \
                 changed_author) VALUES (?1, ?2, 0, '', 1, ?3, 1, 'normal', 'file', 1, 0, 'me')",
            )
            .map_err(e)?;
        for f in 0..20_000 {
            let p = format!("f{f:05}.txt");
            node.execute(rusqlite::params![wc_id, &p, format!("trunk/{p}")])
                .map_err(e)?;
        }
        Ok(())
    })
    .expect("insert");
    (dir, db)
}

#[test]
fn split_read_does_not_deadlock_with_a_committing_writer() {
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) < 2 {
        // One part only: nothing is read on a helper connection.
        return;
    }
    let (dir, db) = build();
    let path = dir.path().join(".svn").join("wc.db");
    let scenario = fail::FailScenario::setup();
    // Helpers start reading only after the writer is waiting to commit.
    fail::cfg("wc.subtree.helper_before_read", "sleep(1500)").expect("cfg");

    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let c = rusqlite::Connection::open(&path).expect("open");
        c.busy_timeout(Duration::from_secs(30)).expect("timeout");
        c.execute_batch(
            "PRAGMA journal_mode = TRUNCATE; BEGIN IMMEDIATE; \
             UPDATE nodes SET changed_author = 'writer' WHERE local_relpath = 'f00000.txt';",
        )
        .expect("write");
        let t = Instant::now();
        c.execute_batch("COMMIT").expect("commit");
        t.elapsed()
    });

    let start = Instant::now();
    let rows = db.read_subtree(&RelPath::root()).expect("read_subtree");
    let took = start.elapsed();
    let waited = writer.join().expect("writer");
    fail::remove("wc.subtree.helper_before_read");
    scenario.teardown();

    assert_eq!(rows.nodes.len(), 20_001, "every row read once");
    assert!(
        took < Duration::from_secs(6),
        "read_subtree waited for the busy timeout: {took:?}"
    );
    assert!(
        waited < Duration::from_secs(6),
        "the writer waited for the busy timeout: {waited:?}"
    );
    // The writer's change landed after the read: the read saw one consistent state.
    assert!(
        rows.nodes
            .iter()
            .filter(|n| !n.relpath.is_root())
            .all(|n| n.changed_author.as_deref() == Some("me"))
    );
}
