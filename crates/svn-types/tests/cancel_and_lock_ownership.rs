//! `CancelToken` and `MemoryLockOwnership`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use svn_types::{CancelToken, ErrorCode, LockOwnershipStore, MemoryLockOwnership, RelPath};

#[test]
fn cancel_is_shared_idempotent_and_checked() {
    let t = CancelToken::new();
    let c = t.clone();
    assert!(!t.is_cancelled());
    assert!(t.check().is_ok());
    c.cancel();
    c.cancel();
    assert!(t.is_cancelled());
    assert_eq!(t.check().expect_err("cancelled").code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn cancelled_future_completes_immediately_when_already_cancelled() {
    let t = CancelToken::new();
    t.cancel();
    tokio::time::timeout(Duration::from_secs(1), t.cancelled())
        .await
        .expect("completes");
}

#[tokio::test]
async fn cancelled_future_wakes_all_waiters() {
    let t = CancelToken::new();
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let t = t.clone();
            tokio::spawn(async move { t.cancelled().await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(waiters.iter().all(|w| !w.is_finished()));
    t.cancel();
    for w in waiters {
        tokio::time::timeout(Duration::from_secs(1), w)
            .await
            .expect("woken")
            .expect("join");
    }
}

#[tokio::test]
async fn cancelled_future_works_in_select() {
    let t = CancelToken::new();
    let c = t.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.cancel();
    });
    tokio::select! {
        () = t.cancelled() => {}
        () = tokio::time::sleep(Duration::from_secs(2)) => panic!("not woken"),
    }
}

#[test]
fn lock_ownership_reports_only_dead_owners() {
    let me = std::process::id();
    let store = MemoryLockOwnership::for_current_process("boot-1");
    let root = Path::new("/wc");
    let alive = RelPath::new("alive").expect("p");
    let other_process = RelPath::new("other/process").expect("p");
    let dead_boot = RelPath::new("dead/boot").expect("p");
    store.record(root, &alive, me, "boot-1").expect("record");
    store
        .record(root, &other_process, me.wrapping_add(1), "boot-1")
        .expect("record");
    store
        .record(root, &dead_boot, me, "boot-0")
        .expect("record");
    store
        .record(Path::new("/other"), &RelPath::new("x").expect("p"), 1, "b")
        .expect("record");

    // A live process of this boot session is never reported, even when it is not this process.
    let dead = store.owned_by_dead_process(root).expect("query");
    assert_eq!(dead, vec![dead_boot.clone()]);

    store.forget(root, &dead_boot).expect("forget");
    assert!(store.owned_by_dead_process(root).expect("query").is_empty());
}

#[test]
fn record_refuses_to_overwrite_a_live_owner_but_replaces_a_dead_one() {
    let store = MemoryLockOwnership::new(Arc::new(|pid: u32, _: &str| pid == 7));
    let root = Path::new("/wc");
    let dir = RelPath::new("a").expect("p");
    store.record(root, &dir, 7, "b").expect("first");
    store.record(root, &dir, 7, "b").expect("same owner again");
    assert_eq!(
        store
            .record(root, &dir, 8, "b")
            .expect_err("live owner")
            .code,
        ErrorCode::WcBusy
    );
    store.forget(root, &dir).expect("forget");
    store.record(root, &dir, 8, "b").expect("free");
    // Pid 8 is dead for this probe, so pid 9 may take over.
    store
        .record(root, &dir, 9, "b")
        .expect("dead owner replaced");
}

#[test]
fn wcroot_is_normalized_lexically() {
    let store = MemoryLockOwnership::new(Arc::new(|_: u32, _: &str| false));
    let dir = RelPath::new("a").expect("p");
    store
        .record(Path::new("/wc/"), &dir, 1, "b")
        .expect("record");
    assert_eq!(
        store.owned_by_dead_process(Path::new("/wc/./")).expect("q"),
        vec![dir.clone()]
    );
    store.forget(Path::new("/wc"), &dir).expect("forget");
    assert!(
        store
            .owned_by_dead_process(Path::new("/wc"))
            .expect("q")
            .is_empty()
    );
}

#[test]
fn lock_ownership_probe_is_injectable() {
    let store = MemoryLockOwnership::new(Arc::new(|pid: u32, _: &str| pid == 7));
    let root = Path::new("/wc");
    store
        .record(root, &RelPath::new("a").expect("p"), 7, "b")
        .expect("record");
    store
        .record(root, &RelPath::new("b").expect("p"), 8, "b")
        .expect("record");
    assert_eq!(
        store.owned_by_dead_process(root).expect("query"),
        vec![RelPath::new("b").expect("p")]
    );
}

#[tokio::test]
async fn dropped_futures_do_not_leave_wakers_behind() {
    let t = CancelToken::new();
    for _ in 0..1000 {
        // Poll once (registers a waker), then drop.
        let r = tokio::time::timeout(Duration::from_millis(0), t.cancelled()).await;
        assert!(r.is_err());
    }
    assert_eq!(t.registered_wakers(), 0);
    // Re-polling the same future from one task registers once.
    let fut = t.cancelled();
    tokio::pin!(fut);
    for _ in 0..10 {
        assert!(
            tokio::time::timeout(Duration::from_millis(0), &mut fut)
                .await
                .is_err()
        );
    }
    assert_eq!(t.registered_wakers(), 1);
}
