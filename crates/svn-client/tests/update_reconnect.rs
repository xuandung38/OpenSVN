//! An update, checkout or remote status whose connection fails mid-drive is resumed on a fresh
//! connection: the working copy keeps the directories the broken drive did not finish
//! `incomplete`, so the next report asks only for the rest.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

use common::*;
use rec_ra::RecRa;
use svn_client::{StatusOptions, checkout, status_remote, update};
use svn_types::{CancelToken, ConnectionFailure, Depth, ErrorCode, Revnum, SvnError};

fn broken(failure: ConnectionFailure) -> SvnError {
    SvnError::new(
        ErrorCode::Network,
        "HTTP request failed: error decoding response body: peer closed connection without \
         sending TLS close_notify",
    )
    .with_connection(failure)
}

/// A repository at r1 with 3 directories of 4 files.
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

fn assert_complete(wc: &std::path::Path, rev: i64, suffix: &str) {
    for d in 0..3 {
        for f in 0..4 {
            assert_eq!(
                read(&wc.join(format!("d{d}/f{f}.txt"))),
                format!("{d}.{f}\n{suffix}"),
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

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_broken_mid_drive_resumes_on_a_fresh_connection() {
    for failure in [
        ConnectionFailure::TlsClosedUncleanly,
        ConnectionFailure::ConnectionReset,
        ConnectionFailure::ConnectionClosedByServer,
        ConnectionFailure::Timeout,
    ] {
        let ra = RecRa::new(seeded());
        ra.update_breaks
            .lock()
            .unwrap()
            .extend([Some((5, broken(failure))), None]);
        let sb = Sandbox::new();
        let wc = sb.path("wc");
        let sum = checkout(
            &ctx(),
            &ra,
            &wc,
            None,
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{failure:?}: {e}"));
        assert_eq!(sum.revision, Some(Revnum(1)));
        assert_complete(&wc, 1, "");
        assert_eq!(
            *ra.update_revisions.lock().unwrap(),
            [Some(Revnum(1)), Some(Revnum(1))],
            "{failure:?}"
        );
        // Every file is reported once, whichever drive brought it in.
        let added = sum
            .events
            .iter()
            .filter(|e| {
                e.action == svn_client::UpdateAction::Add && e.path.as_str().ends_with(".txt")
            })
            .count();
        assert!(added >= 12, "{failure:?}: {added}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_update_stays_at_the_revision_it_was_reaching() {
    let s = seeded();
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
    let mut r2 = Vec::new();
    for d in 0..3 {
        for f in 0..4 {
            r2.push(put(&format!("d{d}/f{f}.txt"), &format!("{d}.{f}\nr2\n")));
        }
    }
    s.seed("bob", "r2", &r2).expect("r2");
    let ra = RecRa::new(s.clone());
    ra.update_breaks.lock().unwrap().extend([
        Some((4, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((3, broken(ConnectionFailure::ConnectionReset))),
        None,
    ]);
    let sum = update(&ctx(), &ra, &wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update resumed twice");
    assert_eq!(sum.revision, Some(Revnum(2)));
    assert_complete(&wc, 2, "r2\n");
    // HEAD for the first drive, then the revision it announced.
    assert_eq!(
        *ra.update_revisions.lock().unwrap(),
        [None, Some(Revnum(2)), Some(Revnum(2))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drive_that_keeps_breaking_is_resumed_while_it_makes_progress() {
    let ra = RecRa::new(seeded());
    {
        let mut breaks = ra.update_breaks.lock().unwrap();
        for _ in 0..5 {
            breaks.push_back(Some((2, broken(ConnectionFailure::TlsClosedUncleanly))));
        }
        breaks.push_back(None);
    }
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &ra,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("every broken drive brought in two more files");
    assert_complete(&wc, 1, "");
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_without_progress_are_bounded_and_the_working_copy_stays_resumable() {
    let s = seeded();
    let ra = RecRa::new(s.clone());
    ra.update_breaks.lock().unwrap().extend([
        // Two files in, then three drives that break before changing anything.
        Some((2, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
        Some((0, broken(ConnectionFailure::TlsClosedUncleanly))),
    ]);
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let err = checkout(
        &ctx(),
        &ra,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("drives that make no progress");
    assert_eq!(
        err.connection_failure(),
        Some(ConnectionFailure::TlsClosedUncleanly)
    );
    // The drive that made progress, then two resumes that made none, and a third broken one.
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 4);
    {
        let db = open_db(&wc);
        assert!(db.work_items().expect("queue").is_empty());
        assert_eq!(
            db.wc_lock_held(&rp("")).expect("lock"),
            svn_wc::db::LockState::Free
        );
    }
    // What the app's "Update to finish" does: a plain update completes the checkout.
    update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("resume");
    assert_complete(&wc, 1, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_connection_failures_are_resumed() {
    for err in [
        SvnError::new(ErrorCode::Network, "connection refused"),
        SvnError::new(ErrorCode::DiskFull, "no space left"),
        SvnError::new(ErrorCode::Cancelled, "cancelled")
            .with_connection(ConnectionFailure::ConnectionReset),
    ] {
        let ra = RecRa::new(seeded());
        ra.update_breaks
            .lock()
            .unwrap()
            .extend([Some((2, err.clone())), None]);
        let sb = Sandbox::new();
        let got = checkout(
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
        assert_eq!(got.code, err.code, "{got}");
        assert_eq!(ra.update_revisions.lock().unwrap().len(), 1, "{err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_status_asks_again_after_a_broken_connection() {
    let s = seeded();
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
    s.seed(
        "bob",
        "r2",
        &[put("d1/f2.txt", "changed\n"), put("d2/new.txt", "n\n")],
    )
    .expect("r2");
    let ra = RecRa::new(s.clone());
    ra.update_breaks.lock().unwrap().extend([
        Some((1, broken(ConnectionFailure::TlsClosedUncleanly))),
        None,
    ]);
    let statuses = status_remote(
        &ctx(),
        &ra,
        &wc,
        &StatusOptions::default(),
        &CancelToken::new(),
    )
    .await
    .expect("remote status after one broken drive");
    assert_eq!(ra.update_revisions.lock().unwrap().len(), 2);
    let remote: Vec<String> = statuses
        .iter()
        .filter(|st| st.remote.is_some())
        .map(|st| st.path.as_str().to_owned())
        .collect();
    assert!(remote.contains(&"d1/f2.txt".to_owned()), "{remote:?}");
    assert!(remote.contains(&"d2/new.txt".to_owned()), "{remote:?}");
    // Nothing was written to the working copy.
    assert_eq!(read(&wc.join("d1/f2.txt")), "1.2\n");
}
