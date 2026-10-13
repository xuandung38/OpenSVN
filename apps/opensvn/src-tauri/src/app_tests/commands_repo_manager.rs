#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Repository manager: checkout into a new child folder, the entries list with missing working
//! copies, locate, rename, remove, reveal and the default checkout folder.

use std::path::Path;
use std::time::Duration;

use opensvn_ipc::{
    CheckoutRequest, Depth, ErrorCode, GrantKind, OpResult, PathGrantId, Rev, WcFormat, WcId,
};

use super::fixture::Fixture;
use crate::commands;
use crate::engine::fake::{DEMO_REPO_URL, demo_wc_root};

fn request(dest: PathGrantId, name: Option<&str>) -> CheckoutRequest {
    CheckoutRequest {
        url: format!("{DEMO_REPO_URL}/trunk"),
        dest,
        dest_name: name.map(str::to_owned),
        rev: None,
        depth: Depth::Infinity,
        sparse: None,
        format: WcFormat::V31,
        store_pristine: true,
        ignore_externals: false,
    }
}

/// Checks out into `parent/name` and returns the registered id.
async fn checkout_into(f: &Fixture, parent: &Path, name: &str) -> WcId {
    let h = commands::checkout(f.state(), request(f.grant_dir(parent), Some(name)))
        .await
        .unwrap();
    match f.wait_op(&h).await.unwrap() {
        OpResult::CheckedOut {
            wc_id: Some(id),
            new_rev,
        } => {
            assert_eq!(new_rev, Rev(10));
            id
        }
        other => panic!("unexpected result {other:?}"),
    }
}

#[tokio::test]
async fn checkout_creates_the_child_folder_registers_it_and_remembers_the_url() {
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let id = checkout_into(&f, parent.path(), "game").await;
    assert!(parent.path().join("game").is_dir());
    let entries = commands::wc_entries(f.state()).await.unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!((e.id.clone(), e.name.as_str()), (id, "game"));
    assert!(!e.missing && e.problem.is_none());
    assert_eq!(e.url.as_deref(), Some(&*format!("{DEMO_REPO_URL}/trunk")));
    assert_eq!(
        Path::new(&e.root_path),
        parent.path().join("game").as_path()
    );
    assert!(!e.added_at.is_empty());
    assert_eq!(
        commands::recent_urls(f.state()).await.unwrap(),
        [format!("{DEMO_REPO_URL}/trunk")]
    );
}

#[tokio::test]
async fn bad_folder_names_are_refused_before_the_grant_is_spent() {
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let grant = f.grant_dir(parent.path());
    for bad in [
        "",
        ".",
        "..",
        "../evil",
        "a/b",
        "a\\b",
        "CON",
        "nul.txt",
        "LPT1",
        "trailing.",
        "trailing ",
        "a:b",
        "q?",
        "bad\0",
    ] {
        let err = commands::checkout(f.state(), request(grant.clone(), Some(bad)))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{bad:?}");
    }
    assert!(
        std::fs::read_dir(parent.path()).unwrap().next().is_none(),
        "nothing was created"
    );
    // the same grant still works with a valid name
    let h = commands::checkout(f.state(), request(grant.clone(), Some("ok")))
        .await
        .unwrap();
    f.wait_op(&h).await.unwrap();
    // and is spent now
    let err = commands::checkout(f.state(), request(grant, Some("again")))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

#[tokio::test]
async fn grant_misuse_and_non_empty_targets_are_refused() {
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    std::fs::create_dir(parent.path().join("full")).unwrap();
    std::fs::write(parent.path().join("full/keep.txt"), b"mine").unwrap();
    std::fs::write(parent.path().join("a-file"), b"x").unwrap();

    // a file grant cannot name a checkout folder
    let file_grant = f.grant_file(&parent.path().join("a-file"));
    let err = commands::checkout(f.state(), request(file_grant, Some("x")))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    // an id the backend never minted (e.g. a raw path) is refused
    let raw = PathGrantId(parent.path().to_string_lossy().into_owned());
    assert_eq!(
        commands::checkout(f.state(), request(raw, Some("x")))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    // an existing non-empty folder, as child or as the target itself
    for (dest, name) in [
        (parent.path().to_path_buf(), Some("full")),
        (parent.path().join("full"), None),
        (parent.path().to_path_buf(), Some("a-file")),
    ] {
        let err = commands::checkout(f.state(), request(f.grant_dir(&dest), name))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{dest:?} {name:?}");
    }
    assert_eq!(
        std::fs::read(parent.path().join("full/keep.txt")).unwrap(),
        b"mine"
    );
    assert!(commands::wc_entries(f.state()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_cancelled_checkout_removes_only_the_folder_it_created() {
    let f = Fixture::new(Duration::from_millis(200)).await;
    let parent = tempfile::tempdir().unwrap();
    std::fs::create_dir(parent.path().join("existing")).unwrap();
    for (name, created) in [("new", true), ("existing", false)] {
        let h = commands::checkout(f.state(), request(f.grant_dir(parent.path()), Some(name)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        commands::op_cancel(f.state(), h.op_id.clone())
            .await
            .unwrap();
        assert_eq!(f.wait_op(&h).await.unwrap_err().code, ErrorCode::Cancelled);
        assert_eq!(parent.path().join(name).exists(), !created, "{name}");
    }
    assert!(commands::wc_entries(f.state()).await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_working_copies_are_listed_and_can_be_located() {
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let id = checkout_into(&f, parent.path(), "proj").await;
    let (old, new) = (parent.path().join("proj"), parent.path().join("moved"));
    std::fs::rename(&old, &new).unwrap();
    f.engine.simulate_moved_wc(&old, Some(&new)).unwrap();

    let e = commands::wc_entries(f.state()).await.unwrap().remove(0);
    assert!(e.missing && e.problem.is_some() && e.url.is_none());
    assert!(
        commands::wc_list(f.state()).await.unwrap().is_empty(),
        "wc_list shows only readable working copies"
    );

    // an unknown id fails before the grant is spent
    let grant = f.grant_dir(&new);
    assert!(
        commands::wc_locate(f.state(), WcId("nope".into()), grant.clone())
            .await
            .is_err()
    );
    // a folder that is not a working copy is refused
    let other = tempfile::tempdir().unwrap();
    assert!(
        commands::wc_locate(f.state(), id.clone(), f.grant_dir(other.path()))
            .await
            .is_err()
    );
    // a folder already registered under another entry is refused
    f.add_demo_wc().await;
    let err = commands::wc_locate(f.state(), id.clone(), f.grant_dir(&demo_wc_root()))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("already in the list"),
        "{}",
        err.message
    );

    let wc = commands::wc_locate(f.state(), id.clone(), grant)
        .await
        .unwrap();
    assert_eq!(Path::new(&wc.root_path), new.as_path());
    let entries = commands::wc_entries(f.state()).await.unwrap();
    let e = entries.iter().find(|e| e.id == id).unwrap();
    assert!(!e.missing && e.last_opened_at.is_some());
}

#[tokio::test]
async fn rename_and_remove_touch_only_the_list() {
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let id = checkout_into(&f, parent.path(), "proj").await;
    commands::wc_rename(f.state(), id.clone(), "  Game client  ".into())
        .await
        .unwrap();
    let e = commands::wc_entries(f.state()).await.unwrap().remove(0);
    assert_eq!(e.name, "Game client");
    assert!(
        parent.path().join("proj").is_dir(),
        "the folder keeps its name"
    );
    for bad in ["", "   ", "tab\tname"] {
        assert!(
            commands::wc_rename(f.state(), id.clone(), bad.into())
                .await
                .is_err()
        );
    }
    commands::wc_remove(f.state(), id.clone()).await.unwrap();
    assert!(commands::wc_entries(f.state()).await.unwrap().is_empty());
    assert!(parent.path().join("proj").is_dir(), "files stay on disk");
    assert!(commands::wc_remove(f.state(), id).await.is_err());
}

#[tokio::test]
async fn reveal_shows_items_inside_the_working_copy_only() {
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let id = checkout_into(&f, parent.path(), "proj").await;
    std::fs::write(parent.path().join("proj/readme.txt"), b"x").unwrap();
    commands::reveal_in_os(f.state(), id.clone(), String::new())
        .await
        .unwrap();
    commands::reveal_in_os(f.state(), id.clone(), "readme.txt".into())
        .await
        .unwrap();
    assert_eq!(
        *f.revealer.revealed.lock().unwrap(),
        [
            parent.path().join("proj"),
            parent.path().join("proj").join("readme.txt")
        ]
    );
    for bad in ["../outside", "/etc", "a/../../b", "missing.txt"] {
        assert!(
            commands::reveal_in_os(f.state(), id.clone(), bad.into())
                .await
                .is_err(),
            "{bad}"
        );
    }
    assert_eq!(f.revealer.revealed.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn default_checkout_folder_is_chosen_natively_and_granted_by_rust() {
    let f = Fixture::new(Duration::ZERO).await;
    // cancelled: unchanged
    let before = commands::checkout_root_choose(f.state()).await.unwrap();
    assert_eq!(before.general_checkout_root, "~/svn");
    // a file is refused
    let root = tempfile::tempdir().unwrap();
    *f.paths.choice.lock().unwrap() = Some((root.path().join("x"), GrantKind::File));
    assert!(commands::checkout_root_choose(f.state()).await.is_err());

    *f.paths.choice.lock().unwrap() = Some((root.path().to_path_buf(), GrantKind::Dir));
    let s = commands::checkout_root_choose(f.state()).await.unwrap();
    assert_eq!(Path::new(&s.general_checkout_root), root.path());

    let grant = commands::checkout_root_grant(f.state()).await.unwrap();
    assert_eq!(grant.kind, GrantKind::Dir);
    assert!(
        !serde_json::to_string(&grant)
            .unwrap()
            .contains(&*root.path().to_string_lossy()),
        "only the display name crosses the boundary"
    );
    let h = commands::checkout(f.state(), request(grant.id, Some("from-default")))
        .await
        .unwrap();
    f.wait_op(&h).await.unwrap();
    assert!(root.path().join("from-default").is_dir());
}
