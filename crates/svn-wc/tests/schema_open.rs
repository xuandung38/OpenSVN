//! Opening existing working copies: wcroot discovery, format policy (31 rw, 32 ro, <31 and >32
//! rejected) and interoperability of an OpenSVN-created wc.db with the real svn client.
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use std::path::Path;
use std::process::Command;

use schema_support::*;
use svn_types::{Depth, ErrorCode};
use svn_wc::db::{NewWc, WcDb, find_wcroot};

fn svn_out(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("svn")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("svn");
    assert!(
        out.status.success(),
        "svn {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn svn_wc_fixture(extra: &[&str]) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = create_repo(tmp.path());
    let wc = checkout(tmp.path(), &url, "wc", extra);
    (tmp, wc, url)
}

#[test]
fn find_wcroot_walks_up_from_files_dirs_and_missing_paths() {
    if skip_without_svn() {
        return;
    }
    let (_tmp, wc, _) = svn_wc_fixture(&[]);
    std::fs::create_dir_all(wc.join("a/b")).expect("mkdir");
    std::fs::write(wc.join("a/b/f.txt"), b"x").expect("write");
    assert_eq!(find_wcroot(&wc).expect("root"), wc);
    assert_eq!(find_wcroot(&wc.join("a/b")).expect("root"), wc);
    assert_eq!(find_wcroot(&wc.join("a/b/f.txt")).expect("root"), wc);
    assert_eq!(
        find_wcroot(&wc.join("a/missing/deleted.txt")).expect("root"),
        wc
    );
}

#[test]
fn find_wcroot_prefers_nested_working_copy_and_rejects_outside() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let outer = tmp.path().join("outer");
    let inner = outer.join("inner");
    for d in [&outer, &inner] {
        std::fs::create_dir_all(d.join(".svn")).expect("mkdir");
        std::fs::write(d.join(".svn/wc.db"), b"").expect("db");
    }
    assert_eq!(find_wcroot(&inner.join("x.txt")).expect("root"), inner);
    assert_eq!(find_wcroot(&outer.join("y.txt")).expect("root"), outer);
    let err = find_wcroot(&tmp.path().join("nowhere/f")).expect_err("not a wc");
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
}

#[test]
fn find_wcroot_reports_pre_1_7_working_copy_as_unsupported() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("old/.svn")).expect("mkdir");
    std::fs::write(tmp.path().join("old/.svn/entries"), b"10\n").expect("entries");
    let err = find_wcroot(&tmp.path().join("old/f.txt")).expect_err("old");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
    assert!(err.message.contains("upgrade"));
}

#[test]
fn opens_svn_format_31_read_write_and_leaves_svn_happy() {
    if skip_without_svn() {
        return;
    }
    let (_tmp, wc, _) = svn_wc_fixture(&[]);
    let db = WcDb::open(&wc).expect("open");
    assert_eq!((db.format(), db.is_writable(), db.wc_id()), (31, true, 1));
    assert!(db.store_pristine());
    // A committed write goes through and a failing closure rolls back.
    db.with_write(|tx| {
        tx.execute("INSERT INTO work_queue (work) VALUES (X'2829')", [])
            .map_err(svn_wc::db::sql_err)?;
        Ok(())
    })
    .expect("write");
    let failed: svn_types::Result<()> = db.with_write(|tx| {
        tx.execute("INSERT INTO work_queue (work) VALUES (X'2829')", [])
            .map_err(svn_wc::db::sql_err)?;
        Err(svn_types::SvnError::new(ErrorCode::Internal, "boom"))
    });
    assert!(failed.is_err());
    let rows: i64 = db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM work_queue", [], |r| r.get(0))
                .map_err(svn_wc::db::sql_err)
        })
        .expect("count");
    assert_eq!(rows, 1);
    db.with_write(|tx| {
        tx.execute("DELETE FROM work_queue", [])
            .map_err(svn_wc::db::sql_err)
            .map(|_| ())
    })
    .expect("cleanup");
    drop(db);
    // Journal mode of the file is unchanged (rollback journal, not WAL) and svn still works.
    let bytes = std::fs::read(wc.join(".svn/wc.db")).expect("read");
    assert_eq!((bytes[18], bytes[19]), (1, 1));
    assert_eq!(svn_out(&wc, &["status"]), "");
    svn_out(&wc, &["cleanup"]);
}

#[test]
fn opens_format_32_read_only_and_refuses_writes() {
    if skip_without_svn_1_15() {
        return;
    }
    let (_tmp, wc, _) = svn_wc_fixture(&["--compatible-version=1.15"]);
    let db = WcDb::open(&wc).expect("open");
    assert_eq!((db.format(), db.is_writable()), (32, false));
    assert!(db.store_pristine());
    let n: i64 = db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
                .map_err(svn_wc::db::sql_err)
        })
        .expect("read");
    assert_eq!(n, 1);
    let err = db.with_write(|_| Ok(())).expect_err("write refused");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
    assert!(err.message.contains("32"));
}

#[test]
fn rejects_formats_below_31_and_above_32_without_touching_them() {
    if skip_without_svn() {
        return;
    }
    for (version, expected_word) in [(30, "upgrade"), (29, "upgrade"), (33, "newer")] {
        let (_tmp, wc, _) = svn_wc_fixture(&[]);
        {
            let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).expect("open");
            c.execute_batch(&format!("PRAGMA user_version = {version};"))
                .expect("set");
        }
        let before = std::fs::read(wc.join(".svn/wc.db")).expect("read");
        let err = WcDb::open(&wc).expect_err("rejected");
        assert_eq!(err.code, ErrorCode::WcFormatUnsupported, "v{version}");
        assert!(
            err.message.contains(expected_word),
            "v{version}: {}",
            err.message
        );
        assert_eq!(
            std::fs::read(wc.join(".svn/wc.db")).expect("read"),
            before,
            "v{version} untouched"
        );
    }
}

#[test]
fn open_without_database_is_not_a_working_copy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = WcDb::open(tmp.path()).expect_err("missing");
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
}

#[test]
fn reinstalls_missing_statistics_table_on_open_like_svn() {
    if skip_without_svn() {
        return;
    }
    let (_tmp, wc, _) = svn_wc_fixture(&[]);
    {
        let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).expect("open");
        c.execute_batch("DROP TABLE sqlite_stat1;").expect("drop");
    }
    drop(WcDb::open(&wc).expect("open"));
    let rows: i64 = open_ro(&wc)
        .query_row("SELECT count(*) FROM sqlite_stat1", [], |r| r.get(0))
        .expect("stat1");
    assert!(rows > 0);
}

#[test]
fn working_copy_created_by_opensvn_is_accepted_by_svn() {
    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let url = create_repo(tmp.path());
    let repo = tmp.path().join("repo");
    let uuid = svn_out(tmp.path(), &["info", "--show-item", "repos-uuid", &url]);
    let wc = tmp.path().join("ours");
    std::fs::create_dir(&wc).expect("mkdir");
    let db = WcDb::create(
        &wc,
        &NewWc {
            repos_root_url: url.clone(),
            repos_uuid: uuid.clone(),
            repos_relpath: String::new(),
            revision: 0,
            depth: Depth::Infinity,
        },
    )
    .expect("create");
    drop(db);
    assert!(repo.is_dir());

    assert_eq!(svn_out(&wc, &["status"]), "");
    assert_eq!(
        svn_out(&wc, &["info", "--show-item", "repos-root-url"]),
        url
    );
    assert_eq!(
        svn_out(&wc, &["info", "--show-item", "wc-root"]),
        wc.canonicalize().expect("c").display().to_string()
    );
    // The WC is fully usable: add, commit, update.
    std::fs::write(wc.join("f.txt"), b"hi\n").expect("write");
    svn_out(&wc, &["add", "-q", "f.txt"]);
    svn_out(&wc, &["commit", "-q", "-m", "x"]);
    svn_out(&wc, &["update", "-q"]);
    assert_eq!(svn_out(&wc, &["status"]), "");
    svn_out(&wc, &["cleanup"]);
    // And it reopens through our API afterwards.
    let db = WcDb::open(&find_wcroot(&wc.join("f.txt")).expect("root")).expect("reopen");
    assert_eq!(db.format(), 31);
}

#[test]
fn create_refuses_existing_database_and_cleans_up_on_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let new = NewWc {
        repos_root_url: "file:///r".into(),
        repos_uuid: "u".into(),
        repos_relpath: String::new(),
        revision: 0,
        depth: Depth::Empty,
    };
    drop(WcDb::create(tmp.path(), &new).expect("first"));
    let err = WcDb::create(tmp.path(), &new).expect_err("second");
    assert_eq!(err.code, ErrorCode::Internal);
    // Failure inside creation (duplicate-free but invalid revision) leaves no wc.db behind.
    let t2 = tempfile::tempdir().expect("tempdir");
    let bad = NewWc {
        revision: u64::MAX,
        ..new
    };
    assert!(WcDb::create(t2.path(), &bad).is_err());
    assert!(!t2.path().join(".svn/wc.db").exists());
}

#[test]
fn create_rejects_pre_1_7_and_foreign_admin_areas_without_touching_them() {
    let new = NewWc {
        repos_root_url: "file:///r".into(),
        repos_uuid: "u".into(),
        repos_relpath: String::new(),
        revision: 0,
        depth: Depth::Empty,
    };
    // Legacy entries file must survive byte for byte.
    let legacy = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(legacy.path().join(".svn")).expect("mkdir");
    std::fs::write(legacy.path().join(".svn/entries"), b"10\nold").expect("entries");
    let err = WcDb::create(legacy.path(), &new).expect_err("legacy");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
    assert_eq!(
        std::fs::read(legacy.path().join(".svn/entries")).expect("read"),
        b"10\nold"
    );
    assert!(!legacy.path().join(".svn/wc.db").exists());
    // An empty .svn directory is rejected the same way and left in place.
    let empty = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(empty.path().join(".svn")).expect("mkdir");
    let err = WcDb::create(empty.path(), &new).expect_err("empty adm");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
    assert!(empty.path().join(".svn").is_dir());
}

#[test]
fn concurrent_creators_have_one_winner_and_the_losers_never_damage_it() {
    let new = NewWc {
        repos_root_url: "file:///r".into(),
        repos_uuid: "u".into(),
        repos_relpath: "trunk".into(),
        revision: 0,
        depth: Depth::Infinity,
    };
    for _ in 0..20 {
        let tmp = tempfile::tempdir().expect("tempdir");
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| WcDb::create(tmp.path(), &new).map(|db| db.wc_id())))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        // The winner's database is intact and openable.
        let db = WcDb::open(tmp.path()).expect("open winner");
        assert_eq!(db.format(), 31);
        let rows: i64 = db
            .with_read(|c| {
                c.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
                    .map_err(svn_wc::db::sql_err)
            })
            .expect("count");
        assert_eq!(rows, 1);
    }
}
