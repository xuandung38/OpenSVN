#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use opensvn_ipc::{AppError, ErrorCode, OpId, OpResult, OperationKind, WcId};

use super::events::RecordingEvents;
use super::manager::HISTORY_LIMIT;
use super::*;
use crate::engine::ProgressUpdate;

fn manager() -> (OperationManager, Arc<RecordingEvents>) {
    let events = Arc::new(RecordingEvents::default());
    (OperationManager::new(events.clone()), events)
}

async fn wait_for(events: &RecordingEvents, prefix: &str, id: &OpId) {
    let want = format!("{prefix}:{}", id.0);
    for _ in 0..500 {
        if events.names().contains(&want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("event {want} not seen; got {:?}", events.names());
}

#[tokio::test]
async fn emits_started_before_finished_and_returns_handle_immediately() {
    let (m, events) = manager();
    let started = Instant::now();
    let h = m.spawn(
        OperationKind::Update,
        Some(WcId("w".into())),
        "Update w",
        |_ctx| async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(OpResult::Done)
        },
    );
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "handle must not wait for the work"
    );
    assert_eq!(events.names()[0], format!("started:{}", h.op_id.0));
    wait_for(&events, "finished", &h.op_id).await;
    assert_eq!(events.names().len(), 2);
}

#[tokio::test]
async fn op_ids_are_uuid_v7_and_unique() {
    let a = new_op_id();
    let b = new_op_id();
    assert_ne!(a, b);
    let parsed = uuid::Uuid::parse_str(&a.0).unwrap();
    assert_eq!(parsed.get_version_num(), 7);
}

#[tokio::test]
async fn failure_emits_failed_with_the_error() {
    let (m, events) = manager();
    let h = m.spawn(OperationKind::Commit, None, "Commit", |_| async {
        Err(AppError::internal("nope"))
    });
    wait_for(&events, "failed", &h.op_id).await;
}

#[tokio::test]
async fn panic_fails_only_that_operation() {
    let (m, events) = manager();
    let bad = m.spawn(OperationKind::Update, None, "bad", |_| async {
        if true {
            panic!("engine bug");
        }
        Ok(OpResult::Done)
    });
    wait_for(&events, "failed", &bad.op_id).await;
    let good = m.spawn(OperationKind::Update, None, "good", |_| async {
        Ok(OpResult::Done)
    });
    wait_for(&events, "finished", &good.op_id).await;
}

#[tokio::test]
async fn cancel_reaches_the_work_in_well_under_a_second() {
    let (m, events) = manager();
    let h = m.spawn(
        OperationKind::Checkout,
        None,
        "Checkout",
        |ctx| async move {
            ctx.cancel.cancelled().await;
            Err(AppError::from(&svn_types::SvnError::new(
                svn_types::ErrorCode::Cancelled,
                "cancelled",
            )))
        },
    );
    let t = Instant::now();
    m.cancel(&h.op_id).unwrap();
    wait_for(&events, "failed", &h.op_id).await;
    assert!(t.elapsed() < Duration::from_secs(1));
    // cancelling again (finished) is fine; an unknown id is not
    m.cancel(&h.op_id).unwrap();
    assert!(m.cancel(&OpId("nope".into())).is_err());
}

#[tokio::test]
async fn progress_is_throttled_to_twenty_per_second() {
    let (m, events) = manager();
    let h = m.spawn(OperationKind::Update, None, "Update", |ctx| async move {
        let start = Instant::now();
        // report as fast as possible for ~350 ms
        while start.elapsed() < Duration::from_millis(350) {
            ctx.progress.report(ProgressUpdate {
                phase: "network".into(),
                current: Some(1),
                ..ProgressUpdate::default()
            });
            tokio::task::yield_now().await;
        }
        Ok(OpResult::Done)
    });
    wait_for(&events, "finished", &h.op_id).await;
    let n = events.progress().len();
    assert!(
        (1..=10).contains(&n),
        "expected about 7 progress events in 350 ms, got {n}"
    );
    assert!(events.progress().iter().all(|p| p.op_id == h.op_id));
}

#[tokio::test]
async fn list_shows_running_then_last_fifty_finished() {
    let (m, events) = manager();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let running = m.spawn(
        OperationKind::Update,
        Some(WcId("w1".into())),
        "long",
        |_| async move {
            let _ = release_rx.await;
            Ok(OpResult::Done)
        },
    );
    let mut last = None;
    for i in 0..60 {
        let h = m.spawn(OperationKind::Log, None, format!("op {i}"), |_| async {
            Ok(OpResult::Done)
        });
        last = Some(h);
    }
    wait_for(&events, "finished", &last.unwrap().op_id).await;
    // let every short operation finish
    for _ in 0..200 {
        if m.list().len() == 1 + HISTORY_LIMIT {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let list = m.list();
    assert_eq!(list.len(), 1 + HISTORY_LIMIT);
    assert_eq!(
        list[0].op_id, running.op_id,
        "running operations come first"
    );
    assert_eq!(list[1].title, "op 59", "newest finished first");
    let _ = release_tx.send(());
}

#[test]
fn error_code_is_cancelled_for_engine_cancel() {
    let e = AppError::from(&svn_types::SvnError::new(
        svn_types::ErrorCode::Cancelled,
        "x",
    ));
    assert_eq!(e.code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn the_last_sample_is_not_lost_to_the_throttle() {
    let (m, events) = manager();
    let h = m.spawn(OperationKind::Update, None, "Update", |ctx| async move {
        for i in 1..=50u64 {
            ctx.progress.report(ProgressUpdate {
                phase: "work-queue".into(),
                current: Some(i),
                total: Some(50),
                ..ProgressUpdate::default()
            });
        }
        Ok(OpResult::Done)
    });
    wait_for(&events, "finished", &h.op_id).await;
    let last = events.progress().last().cloned().unwrap();
    assert_eq!(
        last.current.map(|s| s.0),
        Some(50),
        "UI ends at the final state"
    );
    // and it arrives before the terminal event
    let names = events.names();
    let last_progress = names
        .iter()
        .rposition(|n| n.starts_with("progress:"))
        .unwrap();
    let finished = names
        .iter()
        .position(|n| n.starts_with("finished:"))
        .unwrap();
    assert!(last_progress < finished);
}

#[tokio::test]
async fn notices_are_never_dropped_by_the_throttle() {
    let (m, events) = manager();
    let h = m.spawn(OperationKind::Commit, None, "Commit", |ctx| async move {
        for i in 1..=20u64 {
            ctx.progress.report(ProgressUpdate {
                phase: "commit".into(),
                current: Some(i),
                ..ProgressUpdate::default()
            });
            ctx.progress.report(ProgressUpdate {
                phase: crate::engine::context::PHASE_NESTED_WC_SKIPPED.into(),
                current_path: Some(format!("skipped {i}")),
                ..ProgressUpdate::default()
            });
        }
        Ok(OpResult::Done)
    });
    wait_for(&events, "finished", &h.op_id).await;
    let notices: Vec<_> = events
        .progress()
        .into_iter()
        .filter(|p| p.phase == crate::engine::context::PHASE_NESTED_WC_SKIPPED)
        .collect();
    assert_eq!(notices.len(), 20, "every notice reaches the UI");
}

#[tokio::test]
async fn a_flood_of_paths_is_coalesced_and_every_path_is_counted() {
    use opensvn_ipc::{PathAction, ProgressEntry};
    let (m, events) = manager();
    let h = m.spawn(
        OperationKind::Checkout,
        None,
        "Checkout",
        |ctx| async move {
            for i in 1..=100_000u64 {
                ctx.progress.report(ProgressUpdate {
                    phase: "checkout".into(),
                    current: Some(i),
                    current_path: Some(format!("f{i}")),
                    entry: Some(ProgressEntry {
                        action: if i % 2 == 0 {
                            PathAction::Added
                        } else {
                            PathAction::Updated
                        },
                        path: format!("f{i}"),
                    }),
                    ..ProgressUpdate::default()
                });
            }
            Ok(OpResult::Done)
        },
    );
    wait_for(&events, "finished", &h.op_id).await;
    let progress = events.progress();
    // 100 000 paths reported in a burst: a handful of events, not one per path.
    assert!(progress.len() <= 40, "{} events", progress.len());
    let last = progress.last().unwrap();
    assert_eq!(last.current.map(|s| s.0), Some(100_000));
    let counts = last.counts.unwrap();
    assert_eq!((counts.added, counts.updated), (50_000, 50_000));
    let carried: usize = progress.iter().map(|p| p.entries.len()).sum();
    let dropped: u32 = progress.iter().map(|p| p.entries_dropped).sum();
    assert_eq!(carried + dropped as usize, 100_000);
    assert_eq!(last.entries.last().unwrap().path, "f100000");
}

#[tokio::test]
async fn a_quiet_engine_still_gets_its_last_sample_out_before_the_end() {
    let (m, events) = manager();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let h = m.spawn(OperationKind::Update, None, "Update", |ctx| async move {
        for i in 1..=3u64 {
            ctx.progress.report(ProgressUpdate {
                phase: "update".into(),
                current: Some(i),
                ..ProgressUpdate::default()
            });
        }
        let _ = release_rx.await;
        Ok(OpResult::Done)
    });
    // The ticker sends the held sample although no new one arrives.
    for _ in 0..200 {
        if events
            .progress()
            .last()
            .and_then(|p| p.current)
            .map(|s| s.0)
            == Some(3)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        events
            .progress()
            .last()
            .and_then(|p| p.current)
            .map(|s| s.0),
        Some(3)
    );
    let _ = release_tx.send(());
    wait_for(&events, "finished", &h.op_id).await;
}

#[tokio::test]
async fn running_on_finds_our_writer_of_a_working_copy() {
    let (m, events) = manager();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("wc");
    std::fs::create_dir(&root).unwrap();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let h = m.spawn_with(
        OpSpec::new(OperationKind::Checkout, None, "Checkout wc").at(&root),
        |_| async move {
            let _ = release_rx.await;
            Ok(OpResult::Done)
        },
    );
    assert!(h.root_path.is_some() && !h.background);
    assert_eq!(m.running_on(&root).map(|o| o.op_id), Some(h.op_id.clone()));
    assert!(m.running_on(&root.join("sub")).is_some(), "a folder inside");
    assert!(m.running_on(&dir.path().join("other")).is_none());
    // A background read of the same folder does not hold it.
    let bg = m.spawn_with(
        OpSpec::new(OperationKind::StatusRemote, None, "Check")
            .at(&dir.path().join("other"))
            .in_background(),
        |ctx| async move {
            assert!(!ctx.interactive, "background work never prompts");
            Ok(OpResult::Done)
        },
    );
    assert!(bg.background);
    wait_for(&events, "finished", &bg.op_id).await;
    assert!(m.running_on(&dir.path().join("other")).is_none());
    let _ = release_tx.send(());
    wait_for(&events, "finished", &h.op_id).await;
    assert!(m.running_on(&root).is_none());
    // Background operations stay out of the history.
    assert!(m.list().iter().all(|o| o.op_id != bg.op_id));
    assert!(m.list().iter().any(|o| o.op_id == h.op_id));
}

#[cfg(unix)]
#[tokio::test]
async fn a_checkout_target_not_created_yet_matches_its_folder_once_created() {
    let (m, events) = manager();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    // The user picked the folder through a link (macOS: /tmp is /private/tmp).
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    let target = dir.path().join("link").join("wc");
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let h = m.spawn_with(
        OpSpec::new(OperationKind::Checkout, None, "Checkout wc").at(&target),
        |_| async move {
            let _ = release_rx.await;
            Ok(OpResult::Done)
        },
    );
    std::fs::create_dir(&target).unwrap();
    let registered = dunce::canonicalize(&target).unwrap();
    assert_eq!(
        m.running_on(&registered).map(|o| o.op_id),
        Some(h.op_id.clone())
    );
    let _ = release_tx.send(());
    wait_for(&events, "finished", &h.op_id).await;
}

#[tokio::test]
async fn no_progress_follows_the_end_of_an_operation() {
    let (m, events) = manager();
    let (keep_tx, keep_rx) = tokio::sync::oneshot::channel();
    let h = m.spawn(OperationKind::Update, None, "Update", |ctx| async move {
        let _ = keep_tx.send(ctx.progress.clone());
        Ok(OpResult::Done)
    });
    let reporter = keep_rx.await.unwrap();
    wait_for(&events, "finished", &h.op_id).await;
    reporter.report(ProgressUpdate {
        phase: "update".into(),
        current: Some(1),
        ..ProgressUpdate::default()
    });
    reporter.report(ProgressUpdate {
        phase: "needs-cleanup".into(),
        ..ProgressUpdate::default()
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    let names = events.names();
    assert_eq!(
        names.last().map(String::as_str),
        Some(format!("finished:{}", h.op_id.0).as_str())
    );
}
