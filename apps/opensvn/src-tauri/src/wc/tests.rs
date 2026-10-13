#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use opensvn_ipc::{AppError, ErrorCode};

use super::actor::{RetryPolicy, WcActor};

fn quick() -> RetryPolicy {
    RetryPolicy {
        attempts: 5,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(20),
    }
}

#[tokio::test]
async fn writes_run_one_at_a_time_in_submission_order() {
    let actor = Arc::new(WcActor::spawn(quick()));
    let running = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let (actor, running, max_seen, order) = (
            actor.clone(),
            running.clone(),
            max_seen.clone(),
            order.clone(),
        );
        handles.push(tokio::spawn(async move {
            actor
                .write(async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    order.lock().unwrap().push(i);
                    running.fetch_sub(1, Ordering::SeqCst);
                    Ok(i)
                })
                .await
        }));
        tokio::task::yield_now().await;
    }
    for (i, h) in handles.into_iter().enumerate() {
        assert_eq!(h.await.unwrap().unwrap(), i as u32);
    }
    assert_eq!(max_seen.load(Ordering::SeqCst), 1, "writes overlapped");
    assert_eq!(*order.lock().unwrap(), (0..8).collect::<Vec<_>>());
}

#[tokio::test]
async fn reads_run_in_parallel() {
    let actor = Arc::new(WcActor::spawn(quick()));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let reads = (0..4).map(|_| {
        let (actor, active, peak) = (actor.clone(), active.clone(), peak.clone());
        tokio::spawn(async move {
            actor
                .read(|| {
                    let (active, peak) = (active.clone(), peak.clone());
                    async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await
        })
    });
    for r in reads.collect::<Vec<_>>() {
        r.await.unwrap().unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) > 1, "reads were serialized");
}

#[tokio::test]
async fn read_waits_for_a_running_write_then_succeeds() {
    let actor = Arc::new(WcActor::spawn(RetryPolicy {
        attempts: 20,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(10),
    }));
    let write = {
        let a = actor.clone();
        tokio::spawn(async move {
            a.write(async {
                tokio::time::sleep(Duration::from_millis(60)).await;
                Ok(())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(actor.is_writing());
    let value = actor.read(|| async { Ok(42) }).await.unwrap();
    assert_eq!(value, 42);
    write.await.unwrap().unwrap();
    assert!(!actor.is_writing());
}

#[tokio::test]
async fn read_reports_wc_busy_when_the_writer_never_lets_go() {
    let actor = Arc::new(WcActor::spawn(quick()));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let a = actor.clone();
    let write = tokio::spawn(async move {
        a.write(async {
            let _ = release_rx.await;
            Ok(())
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let err = actor.read(|| async { Ok(1) }).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::WcBusy);
    assert!(err.retryable);
    let _ = release_tx.send(());
    write.await.unwrap().unwrap();
}

#[tokio::test]
async fn read_retries_when_the_job_itself_is_busy() {
    let actor = WcActor::spawn(quick());
    let tries = Arc::new(AtomicU32::new(0));
    let t = tries.clone();
    let out = actor
        .read(move || {
            let t = t.clone();
            async move {
                if t.fetch_add(1, Ordering::SeqCst) < 2 {
                    let mut e = AppError::internal("locked");
                    e.code = ErrorCode::WcBusy;
                    Err(e)
                } else {
                    Ok("done")
                }
            }
        })
        .await
        .unwrap();
    assert_eq!(out, "done");
    assert_eq!(tries.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn panic_in_a_job_does_not_kill_the_actor() {
    let actor = WcActor::spawn(quick());
    let err = actor
        .write(async {
            if true {
                panic!("engine invariant violated");
            }
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    let err = actor
        .read(|| async {
            if true {
                panic!("read bug");
            }
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    // both gates still work
    assert_eq!(actor.write(async { Ok(1) }).await.unwrap(), 1);
    assert_eq!(actor.read(|| async { Ok(2) }).await.unwrap(), 2);
}

#[tokio::test]
async fn cancelling_a_queued_write_returns_at_once_and_the_job_never_runs() {
    use svn_types::CancelToken;
    let actor = Arc::new(WcActor::spawn(quick()));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let a = actor.clone();
    let long = tokio::spawn(async move {
        a.write(async {
            let _ = release_rx.await;
            Ok(())
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let ran = Arc::new(AtomicU32::new(0));
    let cancel = CancelToken::new();
    let (a, r, c) = (actor.clone(), ran.clone(), cancel.clone());
    let queued = tokio::spawn(async move {
        a.write_cancellable(
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            &c,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let t = std::time::Instant::now();
    cancel.cancel();
    let err = queued.await.unwrap().unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(t.elapsed() < Duration::from_millis(500));

    let _ = release_tx.send(());
    long.await.unwrap().unwrap();
    // let the actor reach the abandoned job
    actor.write(async { Ok(()) }).await.unwrap();
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the cancelled job was skipped"
    );
    assert!(!actor.is_writing());
}

#[tokio::test]
async fn a_started_write_keeps_the_caller_waiting_for_its_real_result() {
    use svn_types::CancelToken;
    let actor = WcActor::spawn(quick());
    let cancel = CancelToken::new();
    let c = cancel.clone();
    let canceller = async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        c.cancel();
    };
    let write = actor.write_cancellable(
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok("finished")
        },
        &cancel,
    );
    let (out, ()) = tokio::join!(write, canceller);
    assert_eq!(out.unwrap(), "finished");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_cancel_never_runs_a_job_whose_caller_was_told_it_was_cancelled() {
    use svn_types::CancelToken;
    let actor = Arc::new(WcActor::spawn(quick()));
    for i in 0..300u64 {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel = CancelToken::new();
        let (a, r, c) = (actor.clone(), ran.clone(), cancel.clone());
        let call = tokio::spawn(async move {
            a.write_cancellable(
                async move {
                    r.store(true, Ordering::SeqCst);
                    Ok(())
                },
                &c,
            )
            .await
        });
        // cancel at a slightly different moment each round
        for _ in 0..(i % 7) {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
        let result = call.await.unwrap();
        // give an abandoned job the chance to (wrongly) run
        actor.write(async { Ok(()) }).await.unwrap();
        match result {
            Ok(()) => assert!(
                ran.load(Ordering::SeqCst),
                "round {i}: reported success without running"
            ),
            Err(e) => {
                assert_eq!(e.code, ErrorCode::Cancelled);
                assert!(
                    !ran.load(Ordering::SeqCst),
                    "round {i}: ran although the caller saw a cancel"
                );
            }
        }
    }
}
