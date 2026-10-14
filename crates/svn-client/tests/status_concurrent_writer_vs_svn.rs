//! Local status while another connection writes wc.db (#145). svn 1.15 status only reads: it
//! never needs the write lock and never fails with "database is locked" under a concurrent
//! writer. Neither may ours: neither under a write lock held by another process, nor while svn
//! or our own update keeps writing.
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::oracle::*;
use common::*;
use svn_client::{StatusOptions, checkout, status, update};
use svn_types::{CancelToken, Depth, Revnum};

/// Files in the stress working copy: more NODES rows than the whole-tree read splits at.
const FILES: usize = 5000;

/// r1: `FILES` files in 50 directories; r2 edits one of them.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    for f in 0..FILES {
        write(
            &seed.join(format!("d{:02}/f{f:05}.txt", f % 50)),
            &format!("file {f}\n"),
        );
    }
    let dirs: Vec<String> = (0..50).map(|d| format!("d{d:02}")).collect();
    let mut add = vec!["add", "-q"];
    add.extend(dirs.iter().map(String::as_str));
    repo.svn(&seed, &add);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    write(&seed.join("d00/f00000.txt"), "edited in r2\n");
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// A connection holding wc.db's write lock (RESERVED, with an uncommitted change) until dropped,
/// like a writer in the middle of a transaction.
struct WriteLockHolder(rusqlite::Connection);

impl WriteLockHolder {
    fn take(wc: &Path) -> Self {
        let c = rusqlite::Connection::open(wc.join(".svn/wc.db")).expect("open");
        c.execute_batch(
            "PRAGMA journal_mode = TRUNCATE; BEGIN IMMEDIATE; \
             UPDATE nodes SET changed_author = changed_author WHERE local_relpath = '';",
        )
        .expect("write lock");
        Self(c)
    }
}

impl Drop for WriteLockHolder {
    fn drop(&mut self) {
        let _ = self.0.execute_batch("ROLLBACK");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_does_not_wait_for_the_write_lock_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = repo.checkout("seed");
    write(&seed.join("a/b.txt"), "b\n");
    repo.svn(&seed, &["add", "-q", "a"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    let wc = repo.checkout("wc");
    write(&wc.join("a/b.txt"), "changed\n");

    let held = WriteLockHolder::take(&wc);
    let t = Instant::now();
    let svn = svn_status(&oracle, &wc, &[]);
    assert!(svn.contains("a/b.txt"), "{svn}");
    let ours = status(&ctx(), &wc, &StatusOptions::default(), &CancelToken::new())
        .await
        .expect("status while another process holds the write lock");
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "waited for the write lock: {:?}",
        t.elapsed()
    );
    assert_eq!(ours.len(), 1, "{ours:?}");
    drop(held);
}

/// Runs `status` on `wc` until `stop` is set; returns (runs, failures).
async fn status_until(wc: &Path, stop: &AtomicBool) -> (usize, Vec<String>) {
    let (mut runs, mut failed) = (0, Vec::new());
    while !stop.load(Ordering::Relaxed) || runs < 3 {
        runs += 1;
        if let Err(e) = status(&ctx(), wc, &StatusOptions::default(), &CancelToken::new()).await {
            failed.push(e.to_string());
        }
    }
    (runs, failed)
}

/// Runs `svn status` on `wc` until `stop` is set; returns (runs, failures).
fn svn_status_until(
    oracle: &opensvn_svn_oracle::Oracle,
    wc: &Path,
    stop: &AtomicBool,
) -> (usize, usize) {
    let (mut runs, mut failed) = (0, 0);
    while !stop.load(Ordering::Relaxed) || runs < 3 {
        runs += 1;
        let out = oracle
            .svn()
            .args(["status", "-q"])
            .current_dir(wc)
            .output()
            .expect("svn");
        if !out.status.success() {
            failed += 1;
        }
    }
    (runs, failed)
}

const ROUNDS: usize = 4;

#[tokio::test(flavor = "multi_thread")]
async fn status_under_a_concurrent_svn_update_never_fails_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let wc = repo.checkout("wc");
    let stop = Arc::new(AtomicBool::new(false));
    // The writer: svn moves the working copy between r1 and r2 and edits properties.
    let writer = {
        let (oracle, wc, stop) = (oracle.clone(), wc.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            for i in 0..ROUNDS {
                let rev = if i % 2 == 0 { "1" } else { "2" };
                for args in [
                    vec!["update", "-q", "-r", rev],
                    vec!["propset", "-q", "p", "v", "d01"],
                    vec!["propdel", "-q", "p", "d01"],
                ] {
                    let out = oracle
                        .svn()
                        .args(&args)
                        .current_dir(&wc)
                        .output()
                        .expect("svn");
                    assert!(
                        out.status.success(),
                        "svn {args:?}: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    let (runs, failed) = status_until(&wc, &stop).await;
    writer.join().expect("writer");
    assert!(
        failed.is_empty(),
        "{} of {runs} status runs failed: {failed:?}",
        failed.len()
    );

    // svn under the same load, for reference.
    stop.store(false, Ordering::Relaxed);
    let writer = {
        let (oracle, wc, stop) = (oracle.clone(), wc.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            for i in 0..ROUNDS {
                let rev = if i % 2 == 0 { "1" } else { "2" };
                let out = oracle
                    .svn()
                    .args(["update", "-q", "-r", rev])
                    .current_dir(&wc)
                    .output()
                    .expect("svn");
                assert!(out.status.success());
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    let (svn_runs, svn_failed) = svn_status_until(&oracle, &wc, &stop);
    writer.join().expect("writer");
    assert_eq!(
        svn_failed, 0,
        "svn status failed {svn_failed} of {svn_runs} runs"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_under_our_own_concurrent_update_never_fails() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let session = Arc::new(repo.fake(None));
    checkout(
        &ctx(),
        session.as_ref(),
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (wc, stop, session) = (wc.clone(), Arc::clone(&stop), Arc::clone(&session));
        tokio::spawn(async move {
            for i in 0..ROUNDS {
                let rev = if i % 2 == 0 { 1 } else { 2 };
                update(
                    &ctx(),
                    session.as_ref(),
                    &wc,
                    Some(Revnum(rev)),
                    Depth::Unknown,
                    &CancelToken::new(),
                )
                .await
                .expect("update");
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    let (runs, failed) = status_until(&wc, &stop).await;
    writer.await.expect("writer");
    assert!(
        failed.is_empty(),
        "{} of {runs} status runs failed: {failed:?}",
        failed.len()
    );
    // And svn reads what our update wrote.
    let out = oracle
        .svn()
        .args(["status", "-q"])
        .current_dir(&wc)
        .output()
        .expect("svn");
    assert!(out.status.success());
}
