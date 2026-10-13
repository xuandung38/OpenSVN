//! Open-time robustness: hot journals, a busy database during the optional statistics install,
//! write-protected files and a corrupt format 32 SETTINGS table.
#![allow(clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use rusqlite::Connection;
use svn_types::{Depth, ErrorCode};
use svn_wc::db::{NewWc, WcDb, sql_err};

fn new_wc(dir: &std::path::Path) -> WcDb {
    WcDb::create(
        dir,
        &NewWc {
            repos_root_url: "file:///r".into(),
            repos_uuid: "u".into(),
            repos_relpath: String::new(),
            revision: 0,
            depth: Depth::Infinity,
        },
    )
    .expect("create")
}

fn count(db: &WcDb, table: &str) -> i64 {
    db.with_read(|c| {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .map_err(sql_err)
    })
    .expect("count")
}

/// A crashed writer leaves `wc.db` plus a non-empty `wc.db-journal`. The image is produced by
/// forcing a cache spill inside an open transaction and copying both files before the writer
/// finishes, which is byte-for-byte what a `kill -9` at that moment leaves behind.
#[test]
fn opens_a_working_copy_with_a_hot_journal_like_svn() {
    let src = tempfile::tempdir().expect("tempdir");
    drop(new_wc(src.path()));
    let db_path = src.path().join(".svn/wc.db");

    let crashed = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(crashed.path().join(".svn")).expect("mkdir");
    {
        let writer = Connection::open(&db_path).expect("writer");
        writer
            .execute_batch(
                "PRAGMA cache_size = 10; PRAGMA journal_mode = TRUNCATE; BEGIN IMMEDIATE;",
            )
            .expect("begin");
        for _ in 0..16 {
            writer
                .execute(
                    "INSERT INTO work_queue (work) VALUES (randomblob(1000000))",
                    [],
                )
                .expect("insert");
        }
        let journal = src.path().join(".svn/wc.db-journal");
        let journal_len = std::fs::metadata(&journal).expect("journal exists").len();
        assert!(journal_len > 0, "the transaction must have spilled to disk");
        std::fs::copy(&db_path, crashed.path().join(".svn/wc.db")).expect("copy db");
        std::fs::copy(&journal, crashed.path().join(".svn/wc.db-journal")).expect("copy journal");
        writer.execute_batch("ROLLBACK").expect("rollback");
    }

    let db = WcDb::open(crashed.path()).expect("open with hot journal");
    assert!(db.is_writable());
    assert_eq!(
        count(&db, "work_queue"),
        0,
        "uncommitted rows are rolled back"
    );
    assert_eq!(count(&db, "nodes"), 1);
    db.op_add_probe();
}

trait Probe {
    fn op_add_probe(&self);
}

impl Probe for WcDb {
    /// A write works after recovery.
    fn op_add_probe(&self) {
        self.with_write(|tx| {
            tx.execute("INSERT INTO work_queue (work) VALUES (X'2829')", [])
                .map_err(sql_err)?;
            Ok(())
        })
        .expect("write after recovery");
    }
}

#[test]
fn busy_database_skips_the_statistics_install_quickly_and_atomically() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop(new_wc(tmp.path()));
    let db_path = tmp.path().join(".svn/wc.db");
    let other = Connection::open(&db_path).expect("other");
    other
        .execute_batch("DROP TABLE sqlite_stat1;")
        .expect("drop");

    let holder = Connection::open(&db_path).expect("holder");
    holder
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold write lock");
    let started = Instant::now();
    let db = WcDb::open(tmp.path()).expect("open while another writer is active");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "open waited {:?}",
        started.elapsed()
    );
    drop(db);
    holder.execute_batch("ROLLBACK").expect("release");
    let have: i64 = other
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'sqlite_stat%'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(have, 0, "nothing half-installed");

    // The next open installs the complete hints.
    drop(WcDb::open(tmp.path()).expect("open"));
    let rows: i64 = other
        .query_row("SELECT count(*) FROM sqlite_stat1", [], |r| r.get(0))
        .expect("stat1");
    assert!(rows >= 8, "{rows}");
    let stat4: i64 = other
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'sqlite_stat4'",
            [],
            |r| r.get(0),
        )
        .expect("stat4");
    assert_eq!(stat4, 0);
}

#[cfg(unix)]
#[test]
fn write_protected_database_opens_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    drop(new_wc(tmp.path()));
    let p = tmp.path().join(".svn/wc.db");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    if std::fs::OpenOptions::new().write(true).open(&p).is_ok() {
        return; // running as root: permissions do not apply
    }
    let db = WcDb::open(tmp.path()).expect("open read-only");
    assert!(!db.is_writable());
    assert_eq!(count(&db, "nodes"), 1);
    let err = db.with_write(|_| Ok(())).expect_err("no writes");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
}

#[test]
fn format_32_without_a_settings_row_is_corrupt() {
    let tmp = tempfile::tempdir().expect("tempdir");
    drop(new_wc(tmp.path()));
    {
        let c = Connection::open(tmp.path().join(".svn/wc.db")).expect("open");
        c.execute_batch(
            "CREATE TABLE SETTINGS (wc_id INTEGER NOT NULL REFERENCES WCROOT (id) PRIMARY KEY, store_pristine INTEGER);
             PRAGMA user_version = 32;",
        )
        .expect("fabricate");
    }
    let err = WcDb::open(tmp.path()).expect_err("corrupt");
    assert_eq!(err.code, ErrorCode::Corrupt);
    assert!(err.message.contains("SETTINGS"));
}

#[test]
fn user_version_change_after_open_blocks_writes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = new_wc(tmp.path());
    // The upgrader uses svn's journal mode: a DELETE-mode helper would try to delete the journal
    // file our TRUNCATE-mode connection keeps open, which Windows refuses (SQLITE_IOERR_DELETE).
    Connection::open(tmp.path().join(".svn/wc.db"))
        .expect("other")
        .execute_batch("PRAGMA journal_mode = TRUNCATE; PRAGMA user_version = 32;")
        .expect("upgrade behind our back");
    let err = db.with_write(|_| Ok(())).expect_err("format changed");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
}
