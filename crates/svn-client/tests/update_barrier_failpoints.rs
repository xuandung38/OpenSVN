//! Durability barriers of the batched update editor, driven by the `fail` crate.
//! Run with `cargo test -p svn-client --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, ErrorCode, Revnum};

/// Files with the same text, more than one batch of them (a batch flushes at 2048 nodes).
const SAME_TEXT_FILES: usize = 2300;

// One test function: failpoint configuration is process global.
#[tokio::test(flavor = "multi_thread")]
async fn barriers_of_the_batched_editor() {
    let scenario = fail::FailScenario::setup();
    same_text_is_placed_once_across_batches().await;
    cancel_does_not_wait_for_a_slow_barrier().await;
    scenario.teardown();
}

/// A text placed by one batch is not renamed over by a later batch of the same drive: the
/// earlier batch's barrier covers the stored file and its commit relies on it, while a new
/// inode would only be covered by a later barrier (power loss in between would leave a
/// committed row naming a partial text).
async fn same_text_is_placed_once_across_batches() {
    let s = session();
    let changes: Vec<_> = (0..SAME_TEXT_FILES)
        .map(|i| put(&format!("d{}/f{i}.txt", i % 23), "same text\n"))
        .collect();
    s.seed("a", "r1", &changes).expect("seed");
    let renames = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&renames);
    fail::cfg_callback("pristine.install.before_rename", move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
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
    fail::remove("pristine.install.before_rename");
    assert_eq!(renames.load(Ordering::SeqCst), 1, "one text, placed once");
    assert_eq!(read(&wc.join("d0/f0.txt")), "same text\n");
    let db = open_db(&wc);
    let rows = base_rows(&db);
    assert_eq!(rows.len(), SAME_TEXT_FILES + 23 + 1);
    assert!(rows.iter().all(|r| r.1 == "normal"), "{rows:?}");
}

/// Cancelling returns within the grace period although the barrier of the last batch takes
/// longer; the next update finishes the work.
async fn cancel_does_not_wait_for_a_slow_barrier() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("a.txt", "a\n"),
            put("b.txt", "b\n"),
            put("d/c.txt", "c\n"),
        ],
    )
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
    if !svn_wc::durable::batched_sync(&wc) {
        eprintln!("skipped: the test directory's filesystem syncs every file on its own");
        return;
    }
    s.seed(
        "a",
        "r2",
        &[
            put("a.txt", "a2\n"),
            put("b.txt", "b2\n"),
            put("d/c.txt", "c2\n"),
        ],
    )
    .expect("seed");

    fail::cfg("update.sync_barrier", "sleep(3000)").unwrap();
    let cancel = CancelToken::new();
    let mut c = ctx();
    let trip = cancel.clone();
    c.progress = Some(Arc::new(move |_ev: &svn_client::UpdateEvent| trip.cancel()));
    let started = Instant::now();
    let err = update(&c, &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect_err("cancelled");
    let took = started.elapsed();
    fail::remove("update.sync_barrier");
    assert_eq!(err.code, ErrorCode::Cancelled, "{err}");
    assert!(
        took < Duration::from_millis(1500),
        "cancel waited {took:?} for the barrier"
    );

    let sum = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("resume");
    assert_eq!(sum.revision, Some(Revnum(2)));
    for (f, text) in [("a.txt", "a2\n"), ("b.txt", "b2\n"), ("d/c.txt", "c2\n")] {
        assert_eq!(read(&wc.join(f)), text, "{f}");
    }
    let db = open_db(&wc);
    assert!(db.work_items().expect("queue").is_empty());
    let rows = base_rows(&db);
    assert!(rows.iter().all(|r| r.1 == "normal" && r.3 == 2), "{rows:?}");
}
