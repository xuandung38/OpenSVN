//! `svn cleanup` of another client in the middle of an update drive.
//!
//! svn's cleanup breaks our WC lock by design, vacuums unreferenced pristines (row and file) and
//! empties `.svn/tmp`. An update reuses an unreferenced PRISTINE row without writing the file
//! ("already stored") and references it only when its batch commits, so a vacuum in between
//! used to leave a NODES row whose pristine was gone. The `update.before_batch_txn` failpoint
//! runs exactly that vacuum, deterministically, from a second connection like another process.
//! Run with `cargo test -p svn-client --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, ErrorCode, Revnum};
use svn_wc::pristine::Sha1Digest;

/// Text that r2 gives `b.txt`, stored beforehand as an unreferenced pristine.
const NEW_TEXT: &str = "same as an old pristine\n";

/// r1 checked out with an unreferenced pristine holding [`NEW_TEXT`] (as an earlier operation
/// leaves behind), and r2 changes `b.txt` to that text.
async fn setup() -> (
    svn_ra_api::fake::FakeRaSession,
    Sandbox,
    PathBuf,
    Sha1Digest,
) {
    let s = session();
    s.seed("a", "r1", &[put("a.txt", "a\n"), put("b.txt", "b\n")])
        .expect("seed");
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
    let sha1 = {
        let db = open_db(&wc);
        let mut store = db.pristine_store();
        store
            .install(&mut NEW_TEXT.as_bytes(), None, None)
            .expect("install")
            .sha1
    };
    assert_eq!(refcount(&wc, &sha1), Some(0), "unreferenced row");
    s.seed("a", "r2", &[put("b.txt", NEW_TEXT)]).expect("seed");
    (s, sb, wc, sha1)
}

fn conn(wc: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).expect("open");
    c.busy_timeout(std::time::Duration::from_secs(10))
        .expect("busy timeout");
    c
}

fn refcount(wc: &Path, sha1: &Sha1Digest) -> Option<i64> {
    use rusqlite::OptionalExtension as _;
    conn(wc)
        .query_row(
            "SELECT refcount FROM pristine WHERE checksum = ?1",
            [sha1.to_svn_string()],
            |r| r.get(0),
        )
        .optional()
        .expect("query")
}

fn pristine_path(wc: &Path, sha1: &Sha1Digest) -> PathBuf {
    let hex = sha1.to_hex();
    wc.join(".svn/pristine")
        .join(&hex[..2])
        .join(format!("{hex}.svn-base"))
}

/// What `svn_wc__db_pristine_cleanup` does: every row with refcount 0 goes, then its file.
fn vacuum(wc: &Path) {
    let c = conn(wc);
    let rows: Vec<String> = {
        let mut stmt = c
            .prepare("SELECT checksum FROM pristine WHERE refcount = 0")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };
    for checksum in rows {
        let n = c
            .execute(
                "DELETE FROM pristine WHERE checksum = ?1 AND refcount = 0",
                [&checksum],
            )
            .expect("delete");
        if n > 0 {
            let sha1 = Sha1Digest::from_svn_string(&checksum).expect("sha1");
            let path = pristine_path(wc, &sha1);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The rest of `svn cleanup`: our lock broken (`break_locks`) and `.svn/tmp` emptied, our lock
/// marker and every temp file of the drive with it.
fn break_lock_and_clear_tmp(wc: &Path) {
    conn(wc).execute("DELETE FROM wc_lock", []).expect("break");
    let tmp = wc.join(".svn/tmp");
    for entry in std::fs::read_dir(&tmp).expect("tmp").flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Every text a NODES row names has its PRISTINE row and its file.
fn assert_no_dangling_pristine(wc: &Path, what: &str) {
    let c = conn(wc);
    let mut stmt = c
        .prepare(
            "SELECT n.local_relpath, n.checksum, p.checksum FROM nodes n \
             LEFT JOIN pristine p ON p.checksum = n.checksum WHERE n.checksum IS NOT NULL",
        )
        .expect("prepare");
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert!(!rows.is_empty(), "{what}: no file nodes");
    for (path, checksum, row) in rows {
        assert!(
            row.is_some(),
            "{what}: '{path}' names {checksum} without a row"
        );
        let sha1 = Sha1Digest::from_svn_string(&checksum).expect("sha1");
        assert!(
            pristine_path(wc, &sha1).is_file(),
            "{what}: '{path}' names {checksum} whose file is gone"
        );
    }
}

async fn resume_and_check(s: &svn_ra_api::fake::FakeRaSession, wc: &Path, what: &str) {
    let sum = update(&ctx(), s, wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .unwrap_or_else(|e| panic!("{what}: update again failed: {e}"));
    assert_eq!(sum.revision, Some(Revnum(2)), "{what}");
    check_r2(wc, what);
}

fn check_r2(wc: &Path, what: &str) {
    assert_eq!(read(&wc.join("a.txt")), "a\n", "{what}");
    assert_eq!(read(&wc.join("b.txt")), NEW_TEXT, "{what}");
    let db = open_db(wc);
    let rows = base_rows(&db);
    assert!(
        rows.iter().all(|r| r.1 == "normal" && r.3 == 2),
        "{what}: {rows:?}"
    );
    assert!(db.work_items().expect("queue").is_empty(), "{what}");
    let lock_rows: i64 = conn(wc)
        .query_row("SELECT count(*) FROM wc_lock", [], |r| r.get(0))
        .expect("count");
    assert_eq!(lock_rows, 0, "{what}: lock released");
    drop(db);
    assert_no_dangling_pristine(wc, what);
}

// One test function: failpoint configuration is process global.
#[tokio::test(flavor = "multi_thread")]
async fn svn_cleanup_during_an_update_never_leaves_a_node_without_its_pristine() {
    let scenario = fail::FailScenario::setup();

    // 1. Vacuum only, between `place` and the NODES transaction (svn's cleanup between its
    //    vacuum and its removal of `.svn/tmp`: the lock row is svn's, identical to ours, and our
    //    marker is still there). The transaction finds the reused text gone and restores it from
    //    the copy the drive kept; the update completes.
    {
        let (s, _sb, wc, sha1) = setup().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let (seen, at) = (Arc::clone(&calls), wc.clone());
        fail::cfg_callback("update.before_batch_txn", move || {
            seen.fetch_add(1, Ordering::SeqCst);
            vacuum(&at);
        })
        .expect("cfg");
        let sum = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
            .await
            .expect("update");
        fail::remove("update.before_batch_txn");
        assert!(calls.load(Ordering::SeqCst) > 0, "the vacuum ran");
        assert_eq!(sum.revision, Some(Revnum(2)));
        assert_eq!(refcount(&wc, &sha1), Some(1), "b.txt references it");
        assert!(
            open_db(&wc).pristine_store().verify(&sha1).expect("verify"),
            "restored text matches its checksum"
        );
        check_r2(&wc, "vacuum only");
    }

    // 2. The whole `svn cleanup`: lock broken, pristines vacuumed, `.svn/tmp` emptied (the kept
    //    copy included). The drive stops with a clear error and writes nothing more; the next
    //    update finishes it.
    {
        let (s, _sb, wc, sha1) = setup().await;
        let at = wc.clone();
        fail::cfg_callback("update.before_batch_txn", move || {
            vacuum(&at);
            break_lock_and_clear_tmp(&at);
        })
        .expect("cfg");
        let err = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
            .await
            .expect_err("lock was broken");
        fail::remove("update.before_batch_txn");
        assert_eq!(err.code, ErrorCode::WcBusy, "{err}");
        assert!(
            err.message
                .contains("cleaned up by another client during the update"),
            "{err}"
        );
        assert_eq!(refcount(&wc, &sha1), None, "nothing re-created the row");
        let b = open_db(&wc)
            .read_base(&rp("b.txt"))
            .expect("read")
            .expect("b");
        assert_eq!(b.revision, Some(1), "BASE of b.txt untouched");
        assert_eq!(read(&wc.join("b.txt")), "b\n", "working file untouched");
        assert_no_dangling_pristine(&wc, "after the broken drive");
        resume_and_check(&s, &wc, "whole cleanup").await;
    }

    // 3. Vacuum with the kept copy gone too, while the lock still looks like ours (svn's row,
    //    our marker kept): the transaction cannot restore the text and fails instead of writing
    //    a row without its file.
    {
        let (s, _sb, wc, sha1) = setup().await;
        let at = wc.clone();
        fail::cfg_callback("update.before_batch_txn", move || {
            vacuum(&at);
            let tmp = at.join(".svn/tmp");
            for entry in std::fs::read_dir(&tmp).expect("tmp").flatten() {
                let name = entry.file_name();
                if !name.to_string_lossy().starts_with("opensvn-lock-") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        })
        .expect("cfg");
        let err = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
            .await
            .expect_err("text gone");
        fail::remove("update.before_batch_txn");
        assert_eq!(err.code, ErrorCode::WcBusy, "{err}");
        assert!(err.message.contains(&sha1.to_hex()), "{err}");
        assert_eq!(refcount(&wc, &sha1), None, "the row was rolled back");
        assert_no_dangling_pristine(&wc, "after the failed restore");
        resume_and_check(&s, &wc, "kept copy gone").await;
    }

    scenario.teardown();
}
