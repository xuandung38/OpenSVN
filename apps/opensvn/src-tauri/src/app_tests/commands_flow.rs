#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use opensvn_ipc::{
    CheckoutRequest, CommitRequest, Depth, DiffGetRequest, DiffTarget, ErrorCode, LogGetRequest,
    LogTarget, NodeStatus, OpResult, PathGrantId, Rev, StatusGetRequest, UpdateRequest, WcFormat,
    WcId,
};

use super::fixture::Fixture;
use crate::commands;
use crate::engine::fake::DEMO_REPO_URL;
use crate::ops::events::Recorded;

fn status_req(wc_id: &WcId) -> StatusGetRequest {
    StatusGetRequest {
        wc_id: wc_id.clone(),
        include_ignored: None,
        include_unversioned: None,
    }
}

#[tokio::test]
async fn registry_commands_round_trip() {
    let f = Fixture::new(Duration::ZERO).await;
    assert!(commands::wc_list(f.state()).await.unwrap().is_empty());

    let wc = f.add_demo_wc().await;
    assert_eq!(wc.name, "demo");
    assert_eq!(wc.base_rev, Rev(8));
    assert_eq!(wc.repo_uuid, "6c0f5d2e-0000-4000-8000-0000000000d1");
    // adding again returns the same entry
    assert_eq!(f.add_demo_wc().await.id, wc.id);
    assert_eq!(commands::wc_list(f.state()).await.unwrap().len(), 1);

    commands::wc_rename(f.state(), wc.id.clone(), "Demo project".into())
        .await
        .unwrap();
    assert_eq!(
        commands::wc_open(f.state(), wc.id.clone())
            .await
            .unwrap()
            .name,
        "Demo project"
    );
    commands::wc_close(f.state(), wc.id.clone()).await.unwrap();
    assert!(
        commands::wc_rename(f.state(), wc.id.clone(), "  ".into())
            .await
            .is_err()
    );

    commands::wc_remove(f.state(), wc.id.clone()).await.unwrap();
    assert!(commands::wc_list(f.state()).await.unwrap().is_empty());
    let err = commands::wc_remove(f.state(), wc.id).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
}

#[tokio::test]
async fn adding_something_that_is_not_a_working_copy_fails_with_a_clear_code() {
    let f = Fixture::new(Duration::ZERO).await;
    // a raw path is not accepted any more: only grants from a native dialog or drop
    let err = commands::wc_add_existing(f.state(), PathGrantId("C:\\evil".into()))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    // a file grant where a folder is needed
    let file = f.grant_file(std::path::Path::new("/nowhere"));
    assert_eq!(
        commands::wc_add_existing(f.state(), file)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let elsewhere = if cfg!(windows) {
        r"C:\nowhere"
    } else {
        "/nowhere"
    };
    let err = commands::wc_add_existing(f.state(), f.grant_dir(std::path::Path::new(elsewhere)))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotAWorkingCopy);
    assert_eq!(err.path.as_deref(), Some(elsewhere));
}

#[tokio::test]
async fn status_get_and_background_scan_on_open() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let st = commands::status_get(f.state(), status_req(&wc.id))
        .await
        .unwrap();
    let text = |p: &str| st.iter().find(|s| s.path == p).map(|s| s.text);
    assert_eq!(text("README.md"), Some(NodeStatus::Modified));
    assert_eq!(text("notes.txt"), Some(NodeStatus::Unversioned));

    let without = commands::status_get(
        f.state(),
        StatusGetRequest {
            wc_id: wc.id.clone(),
            include_ignored: None,
            include_unversioned: Some(false),
        },
    )
    .await
    .unwrap();
    assert!(without.iter().all(|s| s.text != NodeStatus::Unversioned));

    // opening scans in the background and announces the first snapshot as a delta
    commands::wc_open(f.state(), wc.id.clone()).await.unwrap();
    for _ in 0..200 {
        if f.events
            .all()
            .iter()
            .any(|r| matches!(r, Recorded::Status(_)))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let delta = f
        .events
        .all()
        .into_iter()
        .find_map(|r| {
            if let Recorded::Status(s) = r {
                Some(s)
            } else {
                None
            }
        })
        .expect("status delta after open");
    assert_eq!(delta.wc_id, wc.id);
    assert!(delta.changed.iter().any(|s| s.path == "README.md"));
    assert!(
        commands::status_get(f.state(), status_req(&WcId("unknown".into())))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn diff_get_covers_working_copy_and_revisions() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let d = commands::diff_get(
        f.state(),
        DiffGetRequest {
            target: DiffTarget::Wc {
                wc_id: wc.id.clone(),
                path: "README.md".into(),
            },
            ignore_whitespace: None,
            force_large: None,
        },
    )
    .await
    .unwrap();
    assert!(d.left.unwrap().content.contains("cargo build`"));
    assert!(d.right.unwrap().content.contains("--release"));

    let d = commands::diff_get(
        f.state(),
        DiffGetRequest {
            target: DiffTarget::Rev {
                url: format!("{DEMO_REPO_URL}/trunk/src/main.rs"),
                rev: Rev(3),
            },
            ignore_whitespace: None,
            force_large: None,
        },
    )
    .await
    .unwrap();
    assert!(d.left.unwrap().content.contains("println!(\"hello\")"));
    assert!(d.right.unwrap().content.contains("hello, world"));
}

#[tokio::test]
async fn commit_operation_emits_lifecycle_and_updates_status_and_history() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    // a first scan gives the publisher a snapshot to compute deltas against
    commands::wc_open(f.state(), wc.id.clone()).await.unwrap();
    for _ in 0..200 {
        if f.events
            .all()
            .iter()
            .any(|r| matches!(r, Recorded::Status(_)))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let t = Instant::now();
    let handle = commands::commit(
        f.state(),
        CommitRequest {
            wc_id: wc.id.clone(),
            paths: vec!["README.md".into()],
            message: "Build in release mode".into(),
            keep_locks: false,
            auto_add_remove: true,
        },
    )
    .await
    .unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(50),
        "command returns the handle quickly"
    );
    assert_eq!(handle.wc_id.as_ref(), Some(&wc.id));
    let OpResult::NewRev { new_rev } = f.wait_op(&handle).await.unwrap() else {
        panic!("commit result is a revision");
    };
    assert_eq!(new_rev, Rev(11));

    let names = f.events.names();
    let started = names
        .iter()
        .position(|n| *n == format!("started:{}", handle.op_id.0))
        .unwrap();
    let finished = names
        .iter()
        .position(|n| *n == format!("finished:{}", handle.op_id.0))
        .unwrap();
    assert!(started < finished);
    // the delta for the committed file arrives after the operation's work
    let st = commands::status_get(f.state(), status_req(&wc.id))
        .await
        .unwrap();
    assert!(st.iter().all(|s| s.path != "README.md"));
    let removed = f.events.all().into_iter().any(|r| match r {
        Recorded::Status(s) => {
            s.changed.iter().any(|c| c.path == "README.md")
                || s.removed.contains(&"README.md".to_owned())
        }
        _ => false,
    });
    assert!(removed, "status delta announced");
    assert!(
        f.events
            .all()
            .iter()
            .any(|r| matches!(r, Recorded::Revision(_)))
    );

    let log = commands::log_get(
        f.state(),
        LogGetRequest {
            target: LogTarget::Wc {
                wc_id: wc.id.clone(),
                path: String::new(),
            },
            from_rev: None,
            to_rev: None,
            limit: 1,
            stop_on_copy: None,
            with_paths: true,
            filter: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(log.entries[0].message, "Build in release mode");
    assert!(log.next_rev.is_some());

    let ops = commands::op_list(f.state()).await.unwrap();
    assert_eq!(ops[0].op_id, handle.op_id);
}

#[tokio::test]
async fn nothing_to_commit_and_conflicting_commit_fail_the_operation() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let h = commands::commit(
        f.state(),
        CommitRequest {
            wc_id: wc.id.clone(),
            paths: vec!["src".into()],
            message: "x".into(),
            keep_locks: false,
            auto_add_remove: true,
        },
    )
    .await
    .unwrap();
    // src/util.rs is added, so this commits it; a second commit has nothing left
    f.wait_op(&h).await.unwrap();
    let h = commands::commit(
        f.state(),
        CommitRequest {
            wc_id: wc.id.clone(),
            paths: vec!["src".into()],
            message: "again".into(),
            keep_locks: false,
            auto_add_remove: true,
        },
    )
    .await
    .unwrap();
    assert!(f.wait_op(&h).await.is_err());
}

#[tokio::test]
async fn status_remote_reports_incoming_and_update_clears_it() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let h = commands::status_remote(f.state(), wc.id.clone())
        .await
        .unwrap();
    f.wait_op(&h).await.unwrap();
    let incoming = f
        .events
        .all()
        .into_iter()
        .find_map(|r| {
            if let Recorded::Incoming(i) = r {
                Some(i)
            } else {
                None
            }
        })
        .expect("incoming event");
    assert_eq!(incoming.count, 2);
    assert_eq!(incoming.latest_rev, Rev(10));
    let listed = commands::wc_list(f.state()).await.unwrap();
    assert_eq!(listed[0].incoming, 2);
    assert!(listed[0].last_checked_at.is_some());

    let h = commands::update(
        f.state(),
        UpdateRequest {
            wc_id: wc.id.clone(),
            paths: None,
            rev: None,
            depth: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        f.wait_op(&h).await.unwrap(),
        OpResult::NewRev { new_rev: Rev(10) }
    );
    assert_eq!(commands::wc_list(f.state()).await.unwrap()[0].incoming, 0);
}

#[tokio::test]
async fn cancelling_a_checkout_ends_the_operation_quickly_and_registers_nothing() {
    let f = Fixture::new(Duration::from_millis(200)).await;
    let target = if cfg!(windows) {
        r"C:\opensvn-fake\cancelled"
    } else {
        "/opensvn-fake/cancelled"
    };
    let h = commands::checkout(
        f.state(),
        CheckoutRequest {
            url: format!("{DEMO_REPO_URL}/trunk"),
            dest: f.grant_dir(std::path::Path::new(target)),
            dest_name: None,
            rev: None,
            depth: Depth::Infinity,
            sparse: None,
            format: WcFormat::V31,
            store_pristine: true,
            ignore_externals: false,
        },
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    commands::op_cancel(f.state(), h.op_id.clone())
        .await
        .unwrap();
    let err = f.wait_op(&h).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(commands::wc_list(f.state()).await.unwrap().is_empty());
    assert!(
        commands::op_cancel(f.state(), opensvn_ipc::OpId("unknown".into()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn checkout_registers_the_new_working_copy_and_rejects_format_32() {
    let f = Fixture::new(Duration::ZERO).await;
    let target = if cfg!(windows) {
        r"C:\opensvn-fake\fresh"
    } else {
        "/opensvn-fake/fresh"
    };
    let mut req = CheckoutRequest {
        url: format!("{DEMO_REPO_URL}/trunk"),
        dest: f.grant_dir(std::path::Path::new(target)),
        dest_name: None,
        rev: Some(Rev(5)),
        depth: Depth::Infinity,
        sparse: None,
        format: WcFormat::V32,
        store_pristine: true,
        ignore_externals: false,
    };
    let err = commands::checkout(f.state(), req.clone())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
    req.format = WcFormat::V31;
    let h = commands::checkout(f.state(), req).await.unwrap();
    let result = f.wait_op(&h).await.unwrap();
    let list = commands::wc_list(f.state()).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "fresh");
    assert_eq!(
        result,
        OpResult::CheckedOut {
            wc_id: Some(list[0].id.clone()),
            new_rev: Rev(5)
        }
    );
}

#[tokio::test]
async fn opening_recovers_dead_owner_locks_and_only_reports_foreign_ones() {
    let f = Fixture::new(Duration::ZERO).await;
    let root = crate::engine::fake::demo_wc_root();
    f.engine.simulate_dead_owner_lock(&root, "src").unwrap();
    let wc = f.add_demo_wc().await;
    assert!(
        !f.events
            .all()
            .iter()
            .any(|r| matches!(r, Recorded::Banner(_))),
        "a dead owner's lock is recovered silently"
    );

    f.engine.simulate_foreign_lock(&root, "docs").unwrap();
    commands::wc_open(f.state(), wc.id.clone()).await.unwrap();
    let banner = f
        .events
        .all()
        .into_iter()
        .find_map(|r| {
            if let Recorded::Banner(b) = r {
                Some(b)
            } else {
                None
            }
        })
        .expect("needs-cleanup banner");
    assert_eq!(banner.kind, opensvn_ipc::BannerKind::NeedsCleanup);
    assert_eq!(banner.data, vec!["docs"]);

    // app start runs the same recovery for every registered working copy
    let before = f.events.all().len();
    f.state().recover_all().await;
    assert!(f.events.all().len() > before);
}
