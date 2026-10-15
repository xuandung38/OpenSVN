//! A checkout or update that loses the network waits for it: it probes the server with a
//! backoff and resumes at the same revision once the server answers, or gives up after the
//! configured wait and leaves the working copy incomplete, ready to be continued.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use rec_ra::RecRa;
use svn_client::netwait::{NetworkWait, WaitState};
use svn_client::{checkout, update};
use svn_types::{CancelToken, ConnectionFailure, Depth, ErrorCode, Revnum, SvnError};

fn broken(failure: ConnectionFailure) -> SvnError {
    SvnError::new(
        ErrorCode::Timeout,
        "HTTP request failed: operation timed out",
    )
    .with_connection(failure)
}

fn unreachable() -> SvnError {
    SvnError::new(
        ErrorCode::Network,
        "HTTP request failed: error sending request: client error (Connect): dns error",
    )
    .with_unreachable()
}

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

type States = Arc<Mutex<Vec<WaitState>>>;

fn waiting_ctx(max_wait: Duration) -> (svn_client::ClientCtx, States, NetworkWait) {
    let states: States = Arc::default();
    let sink = Arc::clone(&states);
    let wait = NetworkWait {
        max_wait,
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(40),
        probe_timeout: Duration::from_secs(5),
        on_state: Some(Arc::new(move |s| sink.lock().unwrap().push(s))),
        ..NetworkWait::default()
    };
    let mut c = ctx();
    c.network_wait = Some(wait.clone());
    (c, states, wait)
}

fn assert_complete(wc: &std::path::Path, rev: i64) {
    for d in 0..3 {
        for f in 0..4 {
            assert_eq!(
                read(&wc.join(format!("d{d}/f{f}.txt"))),
                format!("{d}.{f}\n"),
                "d{d}/f{f}.txt"
            );
        }
    }
    let db = open_db(wc);
    let rows = base_rows(&db);
    assert!(
        rows.iter().all(|r| r.1 == "normal" && r.3 == rev),
        "rows: {rows:?}"
    );
    assert_eq!(rows.len(), 1 + 3 + 12, "rows: {rows:?}");
    assert!(db.work_items().expect("queue").is_empty());
    assert_eq!(
        db.wc_lock_held(&rp("")).expect("lock"),
        svn_wc::db::LockState::Free
    );
}

fn incomplete_rows(wc: &std::path::Path) -> Vec<(String, i64)> {
    base_rows(&open_db(wc))
        .into_iter()
        .filter(|r| r.1 == "incomplete")
        .map(|r| (r.0, r.3))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outage_mid_checkout_is_waited_out_and_the_drive_resumes_at_the_same_revision() {
    let s = seeded();
    let ra = RecRa::new(s.clone());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((5, broken(ConnectionFailure::Timeout))), None]);
    // Three probes find no network, the fourth gets an answer.
    ra.probe_results.lock().unwrap().extend([
        Some(unreachable()),
        Some(broken(ConnectionFailure::Timeout)),
        Some(unreachable()),
        None,
    ]);
    let (c, states, _) = waiting_ctx(Duration::from_secs(60));
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    // HEAD moves on while the network is down: the resume must not follow it.
    s.seed("bob", "r2", &[put("d0/later.txt", "later\n")])
        .expect("r2");
    let sum = checkout(
        &c,
        &ra,
        &wc,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout after the outage");
    assert_eq!(sum.revision, Some(Revnum(1)));
    assert_complete(&wc, 1);
    assert!(!wc.join("d0/later.txt").exists());
    assert_eq!(
        *ra.update_revisions.lock().unwrap(),
        [Some(Revnum(1)), Some(Revnum(1))]
    );
    assert_eq!(ra.probes.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert!(ra.resets.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    let states = states.lock().unwrap().clone();
    let waits = states
        .iter()
        .filter(|s| matches!(s, WaitState::Waiting { .. }))
        .count();
    assert_eq!(waits, 3, "{states:?}");
    assert_eq!(states.last(), Some(&WaitState::Restored), "{states:?}");
    // The backoff grows and is capped.
    let delays: Vec<Duration> = states
        .iter()
        .filter_map(|s| match s {
            WaitState::Waiting { retry_in, .. } => Some(*retry_in),
            _ => None,
        })
        .collect();
    assert_eq!(
        delays,
        [
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(40)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_cannot_be_reached_at_all_is_waited_for_too() {
    // The resume itself cannot connect (DNS gone): without a wait this error is not resumed.
    let ra = RecRa::new(seeded());
    ra.update_breaks.lock().unwrap().extend([
        Some((3, broken(ConnectionFailure::ConnectionReset))),
        Some((0, unreachable())),
        None,
    ]);
    ra.probe_results
        .lock()
        .unwrap()
        .extend([None, Some(unreachable()), None]);
    let (c, _, _) = waiting_ctx(Duration::from_secs(60));
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &c,
        &ra,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    assert_complete(&wc, 1);
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_outage_longer_than_the_wait_leaves_an_incomplete_working_copy_that_update_finishes() {
    let s = seeded();
    let ra = RecRa::new(s.clone());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((5, broken(ConnectionFailure::Timeout)))]);
    *ra.probe_default.lock().unwrap() = Some(unreachable());
    let (c, states, _) = waiting_ctx(Duration::from_millis(150));
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let err = checkout(
        &c,
        &ra,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("the network never came back");
    assert!(err.is_connection_class(), "{err:?}");
    assert_eq!(states.lock().unwrap().last(), Some(&WaitState::GaveUp));
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 1);
    // Incomplete at the checkout's revision, no lock, no queued work, nothing deleted.
    let incomplete = incomplete_rows(&wc);
    assert!(!incomplete.is_empty());
    assert!(
        incomplete.iter().all(|(_, rev)| *rev == 1),
        "{incomplete:?}"
    );
    {
        let db = open_db(&wc);
        assert!(db.work_items().expect("queue").is_empty());
        assert_eq!(
            db.wc_lock_held(&rp("")).expect("lock"),
            svn_wc::db::LockState::Free
        );
    }
    // HEAD moved on; "Continue" updates to the stored revision.
    s.seed("bob", "r2", &[put("d0/later.txt", "later\n")])
        .expect("r2");
    let rev = incomplete.iter().map(|(_, r)| *r).max().unwrap();
    update(
        &ctx(),
        &s,
        &wc,
        Some(Revnum(rev.try_into().unwrap())),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("continue");
    assert_complete(&wc, 1);
    assert!(!wc.join("d0/later.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_now_probes_at_once() {
    let ra = RecRa::new(seeded());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((5, broken(ConnectionFailure::Timeout))), None]);
    ra.probe_results
        .lock()
        .unwrap()
        .extend([Some(unreachable()), None]);
    let (mut c, states, wait) = waiting_ctx(Duration::from_secs(3600));
    // The first wait would take an hour.
    if let Some(w) = c.network_wait.as_mut() {
        w.initial_delay = Duration::from_secs(3600);
        w.max_delay = Duration::from_secs(3600);
    }
    let poke = Arc::clone(&wait.retry_now);
    let watcher = Arc::clone(&states);
    tokio::spawn(async move {
        loop {
            if watcher
                .lock()
                .unwrap()
                .iter()
                .any(|s| matches!(s, WaitState::Waiting { .. }))
            {
                poke.notify_one();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    tokio::time::timeout(
        Duration::from_secs(30),
        checkout(
            &c,
            &ra,
            &wc,
            None,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        ),
    )
    .await
    .expect("retry now did not wake the wait")
    .expect("checkout");
    assert_complete(&wc, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_while_waiting_keeps_the_working_copy_resumable() {
    let s = seeded();
    let ra = RecRa::new(s.clone());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((5, broken(ConnectionFailure::Timeout)))]);
    *ra.probe_default.lock().unwrap() = Some(unreachable());
    let (c, states, _) = waiting_ctx(Duration::from_secs(3600));
    let cancel = CancelToken::new();
    let (watcher, token) = (Arc::clone(&states), cancel.clone());
    tokio::spawn(async move {
        loop {
            if watcher
                .lock()
                .unwrap()
                .iter()
                .filter(|s| matches!(s, WaitState::Waiting { .. }))
                .count()
                >= 2
            {
                token.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let err = checkout(&c, &ra, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(!incomplete_rows(&wc).is_empty());
    assert_eq!(
        open_db(&wc).wc_lock_held(&rp("")).expect("lock"),
        svn_wc::db::LockState::Free
    );
    update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("continue");
    assert_complete(&wc, 1);
}

/// A server or proxy that always cuts the answer at the same point is reachable: the wait does
/// not apply and the drive still stops after two resumes without progress (#164).
#[tokio::test(flavor = "multi_thread")]
async fn a_reachable_server_that_breaks_the_drive_is_not_waited_for() {
    let ra = RecRa::new(seeded());
    ra.update_breaks.lock().unwrap().extend([
        Some((2, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
    ]);
    let (c, states, _) = waiting_ctx(Duration::from_secs(3600));
    let sb = Sandbox::new();
    let err = checkout(
        &c,
        &ra,
        &sb.path("wc"),
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("no progress");
    assert_eq!(
        err.connection_failure(),
        Some(ConnectionFailure::TlsClosedUncleanly)
    );
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 3);
    assert!(
        !states
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WaitState::Waiting { .. })),
    );
}

/// A server that answers the probe but drops off again on every resume is not waited for
/// forever: a few outages in a row without progress end the drive.
#[tokio::test(flavor = "multi_thread")]
async fn outages_without_progress_are_bounded() {
    let ra = RecRa::new(seeded());
    {
        let mut breaks = ra.update_breaks.lock().unwrap();
        breaks.push_back(Some((2, broken(ConnectionFailure::Timeout))));
        for _ in 0..20 {
            breaks.push_back(Some((0, unreachable())));
        }
    }
    {
        // Every first probe after a failure finds no network, the next one does.
        let mut probes = ra.probe_results.lock().unwrap();
        for _ in 0..20 {
            probes.push_back(Some(unreachable()));
            probes.push_back(None);
        }
    }
    let (c, _, _) = waiting_ctx(Duration::from_secs(3600));
    let sb = Sandbox::new();
    let err = checkout(
        &c,
        &ra,
        &sb.path("wc"),
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("bounded");
    assert!(err.is_connection_class(), "{err:?}");
    // The first drive, five resumes after an outage each, then the ordinary resume budget
    // (one more drive; two without progress end it).
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_wait_an_unreachable_server_fails_the_drive_as_before() {
    let ra = RecRa::new(seeded());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((2, unreachable())), None]);
    let sb = Sandbox::new();
    let err = checkout(
        &ctx(),
        &ra,
        &sb.path("wc"),
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("not resumed");
    assert!(err.is_unreachable());
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 1);
    assert_eq!(ra.probes.load(std::sync::atomic::Ordering::SeqCst), 0);
}
