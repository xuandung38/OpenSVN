//! Crash windows of an update that changes the kind of locally changed nodes: a failure injected
//! at every transaction and work queue step leaves a working copy that keeps the user's changes
//! and that the next update completes (nothing queued, nothing incomplete, `.svn/tmp` empty).
//! Run with `cargo test -p svn-client --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};
use svn_wc::db::Presence;

/// r1: file `f`, directory `d` with `in.txt` and `sub/y.txt`, 40 neighbours before them;
/// r2: `f` becomes a directory, `d` a file, the neighbours change and `z.txt` is added.
fn seed() -> svn_ra_api::fake::FakeRaSession {
    let s = session();
    let mut r1 = vec![
        put("f", "orig\n"),
        put("d/in.txt", "in\n"),
        put("d/sub/y.txt", "y\n"),
        prop("f", "fp", Some("file prop")),
    ];
    let mut r2 = vec![
        del("f"),
        dir("f"),
        put("f/inner.txt", "inner\n"),
        prop("f", "nf", Some("dir prop")),
        del("d"),
        put("d", "now a file\n"),
        put("z.txt", "z\n"),
    ];
    for i in 0..40 {
        r1.push(put(&format!("a/{i:02}.txt"), &format!("{i}\n")));
        r2.push(put(&format!("a/{i:02}.txt"), &format!("{i} edited\n")));
    }
    s.seed("a", "r1", &r1).expect("seed r1");
    s.seed("a", "r2", &r2).expect("seed r2");
    s
}

async fn checkout_r1(s: &svn_ra_api::fake::FakeRaSession, wc: &Path) {
    checkout(
        &ctx(),
        s,
        wc,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
}

/// Temp files left in `.svn/tmp`. Lock markers are not counted: a failure injected into the
/// transaction that releases the working copy lock leaves the lock's marker behind by design
/// (see `svn_wc::db::wc_lock`), whatever the update did.
fn tmp_entries(wc: &Path) -> Vec<String> {
    match std::fs::read_dir(wc.join(".svn/tmp")) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with("opensvn-lock-"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// How the working copy was changed before the update, and what must survive it.
#[derive(Debug, Clone, Copy)]
enum Case {
    /// `f` and `d/sub/y.txt` edited, the root updated: both stay as copies with tree conflicts.
    Edited,
    /// `f` deleted locally and updated as the target: the clean update installs the directory.
    DeletedTarget,
}

fn prepare(wc: &Path, case: Case) {
    match case {
        Case::Edited => {
            std::fs::write(wc.join("f"), "orig\nmy edit\n").expect("edit f");
            std::fs::write(wc.join("d/sub/y.txt"), "y\nmy edit\n").expect("edit y");
        }
        Case::DeletedTarget => {
            open_db(wc).op_delete(&rp("f"), &[]).expect("delete f");
            std::fs::remove_file(wc.join("f")).expect("rm f");
        }
    }
}

fn target(wc: &Path, case: Case) -> std::path::PathBuf {
    match case {
        Case::Edited => wc.to_path_buf(),
        Case::DeletedTarget => wc.join("f"),
    }
}

/// The user's edits are on disk, whatever step failed.
fn assert_kept(wc: &Path, case: Case, what: &str) {
    if let Case::Edited = case {
        assert_eq!(read(&wc.join("f")), "orig\nmy edit\n", "{what}: f");
        assert_eq!(read(&wc.join("d/sub/y.txt")), "y\nmy edit\n", "{what}: y");
    }
}

/// A finished update: the edits survive, nothing is queued or incomplete, the neighbours and the
/// replacement reached r2 and `.svn/tmp` is empty.
fn assert_complete(wc: &Path, case: Case, what: &str) {
    assert_kept(wc, case, what);
    let db = open_db(wc);
    assert!(db.work_items().expect("queue").is_empty(), "{what}: queue");
    let rows = base_rows(&db);
    assert!(
        rows.iter().all(|r| r.1 != "incomplete"),
        "{what}: incomplete rows {rows:?}"
    );
    match case {
        Case::Edited => {
            assert_eq!(read(&wc.join("a/39.txt")), "39 edited\n", "{what}");
            assert_eq!(read(&wc.join("z.txt")), "z\n", "{what}");
            for p in ["f", "d"] {
                assert!(
                    db.conflict(&rp(p)).expect("read").is_some(),
                    "{what}: tree conflict on {p}"
                );
            }
        }
        Case::DeletedTarget => {
            // Either the drive installed the directory, or the failed one had already recorded
            // the deletion's tree conflict and the next update skips the conflicted target
            // (svn does the same), leaving the `not-present` marker of the deletion.
            assert!(db.conflict(&rp("f")).expect("read").is_some(), "{what}");
            let f = db.read_base(&rp("f")).expect("read").expect("f");
            if f.presence == Presence::NotPresent {
                assert!(!wc.join("f").exists(), "{what}");
            } else {
                assert_eq!(
                    (f.kind, f.presence),
                    (svn_types::NodeKind::Dir, Presence::Normal),
                    "{what}"
                );
                assert_eq!(read(&wc.join("f/inner.txt")), "inner\n", "{what}");
            }
        }
    }
    assert_eq!(tmp_entries(wc), Vec::<String>::new(), "{what}: .svn/tmp");
}

// One test function: failpoint configuration is process global.
#[tokio::test(flavor = "multi_thread")]
async fn kind_replacements_survive_a_failure_at_every_step() {
    let scenario = fail::FailScenario::setup();
    let sb = Sandbox::new();
    let mut runs = 0;
    for case in [Case::Edited, Case::DeletedTarget] {
        for point in [
            "wc.txn.before_commit",
            "wc.txn.after_commit",
            "wc.wq.after_delete",
            "wc.wq.after_fileinfo",
            "update.after_delete_txn",
            "update.after_txn_before_queue",
        ] {
            // Fail the n-th time the point is reached, for every n until the update gets through.
            // The first transaction obtains the working copy lock: a failure reported after it
            // committed leaves a lock row without its marker, which is foreign by design (see
            // `svn_wc::db::wc_lock`) and needs `cleanup`, so that window is not the update's.
            let first_n = usize::from(point == "wc.txn.after_commit");
            for n in first_n.. {
                let what = format!("{case:?} {point} #{n}");
                let s = seed();
                let wc = sb.path(&format!("{case:?}-{point}-{n}").to_lowercase());
                checkout_r1(&s, &wc).await;
                prepare(&wc, case);
                fail::cfg(point, &format!("{n}*off->1*return->off")).expect("cfg");
                let first = update(
                    &ctx(),
                    &s,
                    &target(&wc, case),
                    Some(Revnum(2)),
                    Depth::Unknown,
                    &CancelToken::new(),
                )
                .await;
                fail::remove(point);
                runs += 1;
                if first.is_ok() {
                    assert_complete(&wc, case, &what);
                    break;
                }
                assert_kept(&wc, case, &format!("{what} (failed)"));
                update(
                    &ctx(),
                    &s,
                    &target(&wc, case),
                    Some(Revnum(2)),
                    Depth::Unknown,
                    &CancelToken::new(),
                )
                .await
                .unwrap_or_else(|e| panic!("{what}: resumed update failed: {e}"));
                assert_complete(&wc, case, &what);
                assert!(n < 500, "{what}: the failpoint never stops firing");
            }
        }
    }
    assert!(runs > 12, "only {runs} runs");
    scenario.teardown();
}
