//! Failures injected on the path of a drive that waited for the network and resumed (the
//! `fail` crate): entering the wait, right after the server answered again, and inside the
//! resumed drive. Each leaves a working copy that is incomplete at the checkout's revision (or
//! complete, when the failure comes after the drive's last commit),
//! unlocked, with no queued work, and that Continue (an update to that revision) finishes.
//! Run with `cargo test -p svn-client --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use rec_ra::RecRa;
use svn_client::netwait::{NetworkWait, WaitState};
use svn_client::{checkout, update};
use svn_types::{CancelToken, ConnectionFailure, Depth, ErrorCode, Revnum, SvnError};

fn seeded() -> svn_ra_api::fake::FakeRaSession {
    let s = session();
    let mut changes = Vec::new();
    for d in 0..3 {
        for f in 0..4 {
            changes.push(put(&format!("d{d}/f{f}.txt"), &format!("{d}.{f}\n")));
        }
    }
    s.seed("alice", "r1", &changes).expect("seed");
    s
}

/// Checks out r1 through a drive that breaks, finds the network down once, resumes, and hits
/// failpoint `fp` (armed when the wait reaches `arm_on`) with `action`.
async fn interrupted(fp: &'static str, action: &'static str, arm_on: fn(&WaitState) -> bool) {
    let s = seeded();
    s.seed("bob", "r2", &[put("d0/later.txt", "later\n")])
        .expect("r2");
    let ra = RecRa::new(s.clone());
    ra.update_breaks.lock().unwrap().extend([
        Some((
            5,
            SvnError::new(ErrorCode::Timeout, "timed out")
                .with_connection(ConnectionFailure::Timeout),
        )),
        None,
    ]);
    ra.probe_results.lock().unwrap().extend([
        Some(SvnError::new(ErrorCode::Network, "dns error").with_unreachable()),
        None,
    ]);
    let mut c = ctx();
    c.network_wait = Some(NetworkWait {
        max_wait: Duration::from_secs(60),
        initial_delay: Duration::from_millis(10),
        on_state: Some(Arc::new(move |st| {
            if arm_on(&st) {
                fail::cfg(fp, action).expect("cfg");
            }
        })),
        ..NetworkWait::default()
    });
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let err = checkout(
        &c,
        &ra,
        &wc,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("injected");
    fail::remove(fp);
    assert!(err.message.contains(fp), "{fp}: {err}");
    {
        let db = open_db(&wc);
        let rows = base_rows(&db);
        // Incomplete at r1, or complete when the failure came after the last commit.
        assert!(
            rows.iter().all(|r| r.1 != "incomplete" || r.3 == 1),
            "{fp}: {rows:?}"
        );
        assert!(db.work_items().expect("queue").is_empty(), "{fp}");
        assert_eq!(
            db.wc_lock_held(&rp("")).expect("lock"),
            svn_wc::db::LockState::Free,
            "{fp}"
        );
    }
    // Continue at the stored revision.
    update(
        &ctx(),
        &s,
        &wc,
        Some(Revnum(1)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .unwrap_or_else(|e| panic!("{fp}: continue failed: {e}"));
    let db = open_db(&wc);
    let rows = base_rows(&db);
    assert_eq!(rows.len(), 1 + 3 + 12, "{fp}: {rows:?}");
    assert!(
        rows.iter().all(|r| r.1 == "normal" && r.3 == 1),
        "{fp}: {rows:?}"
    );
    for d in 0..3 {
        for f in 0..4 {
            assert_eq!(
                read(&wc.join(format!("d{d}/f{f}.txt"))),
                format!("{d}.{f}\n")
            );
        }
    }
    assert!(!wc.join("d0/later.txt").exists(), "{fp}: stayed at r1");
}

// One test function: failpoint configuration is process global.
#[tokio::test(flavor = "multi_thread")]
async fn failures_around_the_network_wait_leave_a_checkout_that_continue_finishes() {
    let scenario = fail::FailScenario::setup();
    // Entering the wait (the drive's lock is held, the queue was run).
    fail::cfg("update.before_network_wait", "1*return").expect("cfg");
    interrupted("update.before_network_wait", "1*return", |_| false).await;
    // The server answered; the resumed drive has not started.
    interrupted("update.after_network_restored", "1*return", |s| {
        matches!(s, WaitState::Restored)
    })
    .await;
    // Inside the resumed drive: before a node's transaction, after it, before its queue runs.
    for fp in [
        "wc.txn.before_commit",
        "wc.txn.after_commit",
        "update.after_txn_before_queue",
        "pristine.install.before_rename",
    ] {
        interrupted(fp, "1*return", |s| matches!(s, WaitState::Restored)).await;
    }
    scenario.teardown();
}
