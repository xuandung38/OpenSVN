#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use opensvn_ipc::{
    BranchKind, CopyLocationRequest, EntryKind, Layout, LockRequest, LockTargets, LogGetRequest,
    LogTarget, OpResult, PathGrantId, RemoteEditRequest, RemoteOp, Rev, UnlockRequest,
};

use super::fixture::Fixture;
use crate::commands;
use crate::engine::fake::DEMO_REPO_URL;

fn url(p: &str) -> String {
    format!("{DEMO_REPO_URL}/{p}")
}

#[tokio::test]
async fn repository_reads_over_the_demo_repository() {
    let f = Fixture::new(Duration::ZERO).await;
    let info = commands::repo_info(f.state(), url("trunk")).await.unwrap();
    assert_eq!(info.youngest_rev, Rev(10));
    assert_eq!(info.kind, EntryKind::Dir);

    let entries = commands::repo_list(f.state(), url("trunk"), None)
        .await
        .unwrap();
    assert_eq!(entries[0].kind, EntryKind::Dir);
    assert!(
        entries
            .iter()
            .any(|e| e.name == "README.md" && e.last_author.is_some())
    );

    let old = commands::repo_cat(f.state(), url("trunk/README.md"), Some(Rev(1)))
        .await
        .unwrap();
    assert_eq!(old.content, "# Demo\n\nA small project.\n");
    assert!(
        commands::repo_cat(f.state(), url("trunk/missing"), None)
            .await
            .is_err()
    );
    assert!(
        commands::repo_info(f.state(), "https://other.test.local/x".into())
            .await
            .is_err()
    );

    let page = commands::log_get(
        f.state(),
        LogGetRequest {
            target: LogTarget::Url { url: url("trunk") },
            from_rev: None,
            to_rev: None,
            limit: 3,
            stop_on_copy: None,
            with_paths: true,
            filter: Some("guide".into()),
        },
    )
    .await
    .unwrap();
    assert!(
        page.entries
            .iter()
            .all(|e| e.message.to_lowercase().contains("guide"))
    );
    assert!(page.entries[0].changed_paths.is_some());

    let branches = commands::branches_list(f.state(), DEMO_REPO_URL.into(), None)
        .await
        .unwrap();
    assert!(branches.iter().any(|b| b.kind == BranchKind::Trunk));
    assert!(
        branches
            .iter()
            .any(|b| b.kind == BranchKind::Branch && b.name == "feature-x")
    );
    assert!(
        branches
            .iter()
            .any(|b| b.kind == BranchKind::Tag && b.name == "v1.0")
    );
}

#[tokio::test]
async fn layout_is_stored_per_repository_and_drives_branch_listing() {
    let f = Fixture::new(Duration::ZERO).await;
    let uuid = "6c0f5d2e-0000-4000-8000-0000000000d1".to_owned();
    assert_eq!(
        commands::layout_get(f.state(), uuid.clone()).await.unwrap(),
        Layout::Auto
    );
    commands::layout_set(f.state(), uuid.clone(), Layout::None)
        .await
        .unwrap();
    assert!(
        commands::branches_list(f.state(), DEMO_REPO_URL.into(), None)
            .await
            .unwrap()
            .is_empty()
    );
    let custom = Layout::Custom {
        trunk: "/{project}/trunk".into(),
        branches: vec!["/{project}/branches".into()],
        tags: vec![],
    };
    commands::layout_set(f.state(), uuid.clone(), custom.clone())
        .await
        .unwrap();
    assert_eq!(
        commands::layout_get(f.state(), uuid.clone()).await.unwrap(),
        custom
    );
    let bad = Layout::Custom {
        trunk: "../x".into(),
        branches: vec![],
        tags: vec![],
    };
    assert!(commands::layout_set(f.state(), uuid, bad).await.is_err());
}

#[tokio::test]
async fn blame_runs_as_an_operation_and_returns_lines() {
    let f = Fixture::new(Duration::ZERO).await;
    let h = commands::blame_get(f.state(), url("trunk/src/main.rs"), None)
        .await
        .unwrap();
    let OpResult::Blame { lines } = f.wait_op(&h).await.unwrap() else {
        panic!("blame result");
    };
    assert!(lines.len() >= 5);
    // the first line survived from the oldest revision's `fn main() {` line
    assert_eq!(lines[0].rev, Rev(1));
    assert!(lines.iter().any(|l| l.rev == Rev(3)));
    assert!(lines.iter().any(|l| l.rev == Rev(10)));
}

#[tokio::test]
async fn remote_edit_copy_and_url_locks() {
    let f = Fixture::new(Duration::ZERO).await;
    let h = commands::remote_edit(
        f.state(),
        RemoteEditRequest {
            ops: vec![
                RemoteOp::Mkdir {
                    url: url("trunk/tools"),
                },
                RemoteOp::Rename {
                    from_url: url("trunk/docs/guide.md"),
                    to_url: url("trunk/tools/guide.md"),
                },
            ],
            message: "Move the guide".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        f.wait_op(&h).await.unwrap(),
        OpResult::NewRev { new_rev: Rev(11) }
    );
    let listed = commands::repo_list(f.state(), url("trunk/tools"), None)
        .await
        .unwrap();
    assert_eq!(listed[0].name, "guide.md");
    assert!(
        commands::repo_cat(f.state(), url("trunk/docs/guide.md"), None)
            .await
            .is_err(),
        "renamed away"
    );
    assert!(
        commands::remote_edit(
            f.state(),
            RemoteEditRequest {
                ops: vec![],
                message: "x".into()
            }
        )
        .await
        .is_err()
    );

    let h = commands::copy_location(
        f.state(),
        CopyLocationRequest {
            from_url: url("trunk"),
            from_rev: None,
            to_url: url("tags/v2.0"),
            message: "Tag 2.0".into(),
            switch_wc_id: None,
            from_wc: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        f.wait_op(&h).await.unwrap(),
        OpResult::NewRev { new_rev: Rev(12) }
    );
    let tags = commands::branches_list(f.state(), DEMO_REPO_URL.into(), None)
        .await
        .unwrap();
    assert!(tags.iter().any(|b| b.name == "v2.0"));

    let target = LockTargets::Url {
        urls: vec![url("trunk/README.md")],
    };
    let h = commands::lock(
        f.state(),
        LockRequest {
            targets: target.clone(),
            comment: Some("c".into()),
            steal: None,
        },
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
    let listed = commands::repo_list(f.state(), url("trunk"), None)
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .find(|e| e.name == "README.md")
            .unwrap()
            .lock
            .is_some()
    );
    let h = commands::unlock(
        f.state(),
        UnlockRequest {
            targets: target,
            break_lock: Some(true),
        },
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
}

#[tokio::test]
async fn repo_save_streams_to_disk_and_refuses_relative_targets() {
    let f = Fixture::new(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("README.md");
    let h = commands::repo_save(
        f.state(),
        url("trunk/README.md"),
        Some(Rev(1)),
        f.grant_file(&out),
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "# Demo\n\nA small project.\n"
    );
    // a folder grant (or a reused grant) cannot be used as the save target
    let again = commands::repo_save(
        f.state(),
        url("trunk/README.md"),
        None,
        PathGrantId("x".into()),
    )
    .await
    .unwrap_err();
    assert_eq!(again.code, opensvn_ipc::ErrorCode::InvalidArgument);
    let wrong_kind = f.grant_dir(dir.path());
    assert!(
        commands::repo_save(f.state(), url("trunk/README.md"), None, wrong_kind)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn working_copy_edits_and_depth_through_commands() {
    use opensvn_ipc::{
        CleanupRequest, ErrorCode, IgnoreAddRequest, IgnoreScope, NodeStatus, RevertRequest,
        StatusGetRequest,
    };
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let status = |wc_id: opensvn_ipc::WcId| async {
        commands::status_get(
            f.state(),
            StatusGetRequest {
                wc_id,
                include_ignored: Some(true),
                include_unversioned: None,
            },
        )
        .await
        .unwrap()
    };

    commands::ignore_add(
        f.state(),
        IgnoreAddRequest {
            wc_id: wc.id.clone(),
            pattern: "*.txt".into(),
            dir: String::new(),
            scope: IgnoreScope::Local,
        },
    )
    .await
    .unwrap();
    let st = status(wc.id.clone()).await;
    assert_eq!(
        st.iter().find(|s| s.path == "notes.txt").unwrap().text,
        NodeStatus::Ignored
    );

    commands::changelist_set(
        f.state(),
        wc.id.clone(),
        vec!["src".into()],
        Some("feature".into()),
    )
    .await
    .unwrap();
    commands::props_set(
        f.state(),
        wc.id.clone(),
        "README.md".into(),
        "svn:needs-lock".into(),
        Some("*".into()),
    )
    .await
    .unwrap();
    assert!(
        status(wc.id.clone())
            .await
            .iter()
            .find(|s| s.path == "README.md")
            .unwrap()
            .needs_lock
    );

    let h = commands::revert(
        f.state(),
        RevertRequest {
            wc_id: wc.id.clone(),
            paths: vec!["README.md".into()],
            remove_unversioned: None,
        },
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
    let h = commands::delete_paths(f.state(), wc.id.clone(), vec!["docs".into()], None)
        .await
        .unwrap();
    f.wait_op(&h).await.unwrap();
    let h = commands::move_path(
        f.state(),
        wc.id.clone(),
        "src/lib.rs".into(),
        "src/core.rs".into(),
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
    let st = status(wc.id.clone()).await;
    assert_eq!(
        st.iter().find(|s| s.path == "src/core.rs").unwrap().text,
        NodeStatus::Added
    );
    assert_eq!(
        st.iter().find(|s| s.path == "src/lib.rs").unwrap().text,
        NodeStatus::Deleted
    );
    commands::mark_move(
        f.state(),
        wc.id.clone(),
        "src/lib.rs".into(),
        "src/core.rs".into(),
    )
    .await
    .unwrap();

    let h = commands::cleanup(
        f.state(),
        CleanupRequest {
            wc_id: wc.id.clone(),
            remove_unversioned: Some(true),
            remove_ignored: Some(true),
            vacuum_pristines: None,
        },
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
    assert!(
        status(wc.id.clone())
            .await
            .iter()
            .all(|s| s.text != NodeStatus::Ignored)
    );

    let h = commands::set_depth(
        f.state(),
        wc.id.clone(),
        "docs".into(),
        opensvn_ipc::Depth::Empty,
    )
    .await
    .unwrap();
    assert!(
        f.wait_op(&h).await.is_err(),
        "docs was deleted from the working copy listing"
    );

    let h = commands::upgrade_format(f.state(), wc.id.clone(), 32)
        .await
        .unwrap();
    f.wait_op(&h).await.unwrap();
    assert_eq!(
        commands::wc_list(f.state()).await.unwrap()[0].format,
        opensvn_ipc::WcFormat::V32
    );
    let err = commands::upgrade_format(f.state(), wc.id.clone(), 30)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
}

#[tokio::test]
async fn layout_detect_finds_the_demo_layout_and_the_current_branch() {
    let f = Fixture::new(Duration::ZERO).await;
    let d = commands::layout_detect(
        f.state(),
        DEMO_REPO_URL.into(),
        url("branches/feature-x/src"),
    )
    .await
    .unwrap();
    assert!(d.found);
    assert_eq!(d.layout, Some(Layout::Auto));
    assert_eq!(d.current.unwrap().name, "feature-x");
    let d = commands::layout_detect(f.state(), DEMO_REPO_URL.into(), url("trunk"))
        .await
        .unwrap();
    assert_eq!(d.current.unwrap().kind, BranchKind::Trunk);
    assert!(
        commands::layout_detect(
            f.state(),
            "https://nowhere.test.local/x".into(),
            "https://nowhere.test.local/x".into()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn copy_from_a_working_copy_includes_local_changes() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let h = commands::copy_location(
        f.state(),
        CopyLocationRequest {
            from_url: String::new(),
            from_rev: None,
            to_url: url("branches/from-wc"),
            message: "Branch from working copy".into(),
            switch_wc_id: None,
            from_wc: Some(wc.id.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        f.wait_op(&h).await.unwrap(),
        OpResult::NewRev { new_rev: Rev(11) }
    );
    // the modified README (local change) and the added file are in the copy; the missing file is not
    let readme = commands::repo_cat(f.state(), url("branches/from-wc/README.md"), None)
        .await
        .unwrap();
    assert!(readme.content.contains("--release"));
    assert!(
        commands::repo_cat(f.state(), url("branches/from-wc/src/util.rs"), None)
            .await
            .is_ok()
    );
    assert!(
        commands::repo_cat(f.state(), url("branches/from-wc/docs/guide.md"), None)
            .await
            .is_err()
    );
    assert!(
        commands::repo_cat(f.state(), url("branches/from-wc/notes.txt"), None)
            .await
            .is_err(),
        "unversioned files stay out"
    );
}

#[tokio::test]
async fn log_paging_walks_every_revision_once_newest_first() {
    let f = Fixture::new(Duration::ZERO).await;
    let mut from = None;
    let mut seen = Vec::new();
    for _ in 0..10 {
        let page = commands::log_get(
            f.state(),
            LogGetRequest {
                target: LogTarget::Url { url: url("") },
                from_rev: from,
                to_rev: None,
                limit: 3,
                stop_on_copy: None,
                with_paths: false,
                filter: None,
            },
        )
        .await
        .unwrap();
        assert!(page.entries.len() <= 3);
        seen.extend(page.entries.iter().map(|e| e.rev.0));
        match page.next_rev {
            Some(next) => {
                // nextRev is the first revision not returned yet
                assert_eq!(next.0, page.entries.last().unwrap().rev.0 - 1);
                from = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(seen, (1..=10).rev().collect::<Vec<u64>>());
}

#[tokio::test]
async fn a_lock_held_elsewhere_fails_with_its_owner_and_only_an_explicit_steal_takes_it() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    // Someone locks README.md straight in the repository: this working copy has no token.
    let h = commands::lock(
        f.state(),
        LockRequest {
            targets: LockTargets::Url {
                urls: vec![url("trunk/README.md")],
            },
            comment: Some("held by the other checkout".into()),
            steal: None,
        },
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();

    let wc_target = LockTargets::Wc {
        wc_id: wc.id.clone(),
        paths: vec!["README.md".into()],
    };
    let h = commands::lock(
        f.state(),
        LockRequest {
            targets: wc_target.clone(),
            comment: None,
            steal: None,
        },
    )
    .await
    .unwrap();
    let err = f.wait_op(&h).await.unwrap_err();
    assert_eq!(err.code, opensvn_ipc::ErrorCode::LockedByOther);
    assert_eq!(
        err.actions,
        Some(vec![opensvn_ipc::ErrorAction::OpenLockDialog])
    );
    let conflicts = err.lock_conflicts.expect("the refusal names the lock");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0].path, "README.md");
    assert!(!conflicts[0].owner.is_empty());
    assert_eq!(
        conflicts[0].comment.as_deref(),
        Some("held by the other checkout")
    );
    assert!(conflicts[0].created_at.is_some());

    let h = commands::lock(
        f.state(),
        LockRequest {
            targets: wc_target,
            comment: Some("mine now".into()),
            steal: Some(true),
        },
    )
    .await
    .unwrap();
    f.wait_op(&h).await.unwrap();
}

#[tokio::test]
async fn other_lock_failures_carry_no_lock_conflicts() {
    let f = Fixture::new(Duration::ZERO).await;
    let h = commands::lock(
        f.state(),
        LockRequest {
            targets: LockTargets::Url {
                urls: vec![url("trunk/does-not-exist")],
            },
            comment: None,
            steal: None,
        },
    )
    .await
    .unwrap();
    let err = f.wait_op(&h).await.unwrap_err();
    assert_ne!(err.code, opensvn_ipc::ErrorCode::LockedByOther);
    assert!(err.lock_conflicts.is_none());
}
