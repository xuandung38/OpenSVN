#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::Path;

use svn_types::{Depth, ErrorCode};

use super::*;

fn new_wc() -> NewWc {
    NewWc {
        repos_root_url: "file:///r".into(),
        repos_uuid: "u".into(),
        repos_relpath: "trunk".into(),
        revision: 7,
        depth: Depth::Infinity,
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// What a kill before the rename leaves: tmp with the half-built database, its journal and the
/// lock file, an empty pristine directory.
fn skeleton(root: &Path, db_bytes: &[u8]) {
    let tmp = root.join(".svn/tmp");
    fs::create_dir_all(&tmp).unwrap();
    fs::create_dir(root.join(".svn/pristine")).unwrap();
    fs::write(tmp.join(CREATING_DB), db_bytes).unwrap();
    fs::write(tmp.join(CREATING_JOURNAL), b"").unwrap();
    fs::write(tmp.join(CREATING_LOCK), b"").unwrap();
}

fn assert_fresh(root: &Path) {
    let db = WcDb::open(root).expect("open");
    assert_eq!(db.format(), FORMAT_WRITE);
    assert_eq!(
        names(&root.join(".svn")),
        ["entries", "format", "pristine", "tmp", "wc.db"]
    );
    assert!(names(&root.join(".svn/tmp")).is_empty(), "tmp left over");
}

#[test]
fn create_leaves_only_the_final_layout() {
    let t = tempfile::tempdir().unwrap();
    drop(WcDb::create(t.path(), &new_wc()).expect("create"));
    assert_fresh(t.path());
}

#[test]
fn skeleton_of_a_killed_creation_is_replaced() {
    for bytes in [&b""[..], b"SQLite format 3\0 half written"] {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("user.txt"), b"mine").unwrap();
        skeleton(t.path(), bytes);
        drop(WcDb::create(t.path(), &new_wc()).expect("restart"));
        assert_fresh(t.path());
        assert_eq!(fs::read(t.path().join("user.txt")).unwrap(), b"mine");
    }
}

#[test]
fn empty_in_place_database_of_earlier_versions_is_an_interrupted_checkout() {
    for with_schema_less_db in [false, true] {
        let t = tempfile::tempdir().unwrap();
        let adm = t.path().join(".svn");
        fs::create_dir_all(adm.join("tmp")).unwrap();
        fs::create_dir(adm.join("pristine")).unwrap();
        if with_schema_less_db {
            // A valid SQLite file without any schema object (e.g. only a pragma ran).
            let c = rusqlite::Connection::open(adm.join(DB_FILE)).unwrap();
            c.execute_batch("PRAGMA page_size = 4096; VACUUM;").unwrap();
        } else {
            fs::write(adm.join(DB_FILE), b"").unwrap();
        }
        fs::write(adm.join(DB_JOURNAL), b"").unwrap();
        let err = WcDb::open(t.path()).expect_err("not a working copy");
        assert_eq!(err.code, ErrorCode::NotAWorkingCopy, "{}", err.message);
        assert!(
            err.message.contains("interrupted checkout"),
            "{}",
            err.message
        );
        assert!(!err.message.contains("upgrade"), "{}", err.message);
        drop(WcDb::create(t.path(), &new_wc()).expect("restart"));
        assert_fresh(t.path());
    }
}

#[test]
fn anything_else_in_the_admin_area_is_never_removed() {
    type Extra = fn(&Path);
    let cases: [(&str, Extra); 6] = [
        ("user file in .svn", |r| {
            fs::write(r.join(".svn/notes.txt"), b"keep").unwrap();
        }),
        ("unknown file in tmp", |r| {
            fs::write(r.join(".svn/tmp/svn-AbCd"), b"keep").unwrap();
        }),
        ("pristine text", |r| {
            fs::create_dir(r.join(".svn/pristine/ab")).unwrap();
        }),
        ("format file", |r| {
            fs::write(r.join(".svn/format"), b"12\n").unwrap();
        }),
        ("database with a schema", |r| {
            let c = rusqlite::Connection::open(r.join(".svn/wc.db")).unwrap();
            c.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
                .unwrap();
        }),
        ("not a database", |r| {
            fs::write(r.join(".svn/wc.db"), b"precious bytes, not sqlite").unwrap();
        }),
    ];
    for (what, extra) in cases {
        let t = tempfile::tempdir().unwrap();
        skeleton(t.path(), b"");
        extra(t.path());
        let before = snapshot(&t.path().join(".svn"));
        let err = WcDb::create(t.path(), &new_wc()).expect_err(what);
        assert_ne!(err.code, ErrorCode::WcBusy, "{what}");
        assert_eq!(snapshot(&t.path().join(".svn")), before, "{what}: touched");
    }
}

#[test]
fn a_tmp_with_unknown_files_is_not_a_skeleton() {
    // `.svn/tmp` with no lock file and no database but something else in `tmp`, or next to it
    // an empty `pristine` (made only under the lock): never taken over.
    type Extra = fn(&Path);
    let cases: [(&str, Extra); 2] = [
        ("unknown file in tmp", |r| {
            fs::write(r.join(".svn/tmp/svn-AbCd"), b"keep").unwrap();
        }),
        ("pristine without lock", |r| {
            fs::create_dir(r.join(".svn/pristine")).unwrap();
        }),
    ];
    for (what, extra) in cases {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".svn/tmp")).unwrap();
        extra(t.path());
        let before = snapshot(&t.path().join(".svn"));
        let err = WcDb::create(t.path(), &new_wc()).expect_err(what);
        assert_eq!(
            err.code,
            ErrorCode::WcFormatUnsupported,
            "{what}: {}",
            err.message
        );
        assert_eq!(snapshot(&t.path().join(".svn")), before, "{what}");
    }
}

#[test]
fn an_empty_tmp_alone_is_taken_over() {
    // A creator killed between its `mkdir` of `.svn/tmp` and the lock file.
    let t = tempfile::tempdir().unwrap();
    fs::create_dir_all(t.path().join(".svn/tmp")).unwrap();
    drop(WcDb::create(t.path(), &new_wc()).expect("restart"));
    assert_fresh(t.path());
}

#[test]
fn an_empty_admin_dir_is_taken_over() {
    // A creator killed between its `mkdir` of `.svn` and that of `.svn/tmp` leaves an empty
    // `.svn`, which holds nothing to lose: the next creation builds the working copy in it.
    let t = tempfile::tempdir().unwrap();
    fs::write(t.path().join("user.txt"), b"mine").unwrap();
    fs::create_dir(t.path().join(".svn")).unwrap();
    drop(WcDb::create(t.path(), &new_wc()).expect("restart"));
    assert_fresh(t.path());
    assert_eq!(fs::read(t.path().join("user.txt")).unwrap(), b"mine");
}

#[cfg(unix)]
#[test]
fn a_symlink_to_an_empty_dir_is_not_taken_over() {
    let t = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), t.path().join(".svn")).unwrap();
    assert!(WcDb::create(t.path(), &new_wc()).is_err());
    assert!(names(outside.path()).is_empty());
}

#[test]
fn skeleton_of_a_live_creator_is_left_alone() {
    let t = tempfile::tempdir().unwrap();
    skeleton(t.path(), b"");
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(t.path().join(".svn/tmp").join(CREATING_LOCK))
        .unwrap();
    lock.lock().unwrap();
    let before = snapshot(&t.path().join(".svn"));
    let err = WcDb::create(t.path(), &new_wc()).expect_err("busy");
    assert_eq!(err.code, ErrorCode::WcBusy, "{}", err.message);
    assert_eq!(snapshot(&t.path().join(".svn")), before);
    lock.unlock().unwrap();
    drop(WcDb::create(t.path(), &new_wc()).expect("after the creator died"));
    assert_fresh(t.path());
}

#[cfg(unix)]
#[test]
fn symlinked_admin_dirs_are_never_followed() {
    let t = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join(CREATING_DB), b"").unwrap();
    fs::create_dir(t.path().join(".svn")).unwrap();
    std::os::unix::fs::symlink(outside.path(), t.path().join(".svn/tmp")).unwrap();
    assert!(WcDb::create(t.path(), &new_wc()).is_err());
    assert!(outside.path().join(CREATING_DB).exists());
}

/// Path, kind and contents of everything below `dir`.
fn snapshot(dir: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
                out.push((rel, None));
            } else if p.file_name().is_some_and(|n| n == CREATING_LOCK) {
                // Only its lock counts; a held lock bars reading it on Windows.
                out.push((rel, Some(Vec::new())));
            } else {
                out.push((rel, Some(fs::read(&p).unwrap())));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn concurrent_takeovers_of_a_skeleton_have_one_winner() {
    for (legacy, empty, bare_tmp) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        for _ in 0..20 {
            let t = tempfile::tempdir().unwrap();
            if empty {
                fs::create_dir(t.path().join(".svn")).unwrap();
            } else if bare_tmp {
                fs::create_dir_all(t.path().join(".svn/tmp")).unwrap();
            } else if legacy {
                fs::create_dir_all(t.path().join(".svn/tmp")).unwrap();
                fs::write(t.path().join(".svn").join(DB_FILE), b"").unwrap();
            } else {
                skeleton(t.path(), b"");
            }
            let results: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..4)
                    .map(|_| s.spawn(|| WcDb::create(t.path(), &new_wc()).map(|db| db.wc_id())))
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert_eq!(
                results.iter().filter(|r| r.is_ok()).count(),
                1,
                "{results:?}"
            );
            assert_fresh(t.path());
        }
    }
}
