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
        ".svn",
        "_SVN.",
        "COM0",
        "a\u{202E}b",
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
    // a working copy of another repository (different UUID) is refused, and the entry keeps
    // its old root and root URL
    let before = commands::wc_entries(f.state()).await.unwrap().remove(0);
    f.engine
        .simulate_repository(&new, Some("00000000-0000-4000-8000-00000000beef"), None)
        .unwrap();
    let err = commands::wc_locate(f.state(), id.clone(), f.grant_dir(&new))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("another repository"),
        "{}",
        err.message
    );
    let e = commands::wc_entries(f.state()).await.unwrap().remove(0);
    assert_eq!(Path::new(&e.root_path), old.as_path());
    assert_eq!(e.repo_root_url, before.repo_root_url);
    assert!(e.missing);
    // same repository (UUID), but relocated to another root URL while it was missing: accepted
    // below, and the entry takes the new root URL
    f.engine
        .simulate_repository(
            &new,
            Some(&before.repo_uuid),
            Some("https://relocated.example/svn/demo"),
        )
        .unwrap();
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
    assert_eq!(wc.repo_root_url, "https://relocated.example/svn/demo");
    let entries = commands::wc_entries(f.state()).await.unwrap();
    let e = entries.iter().find(|e| e.id == id).unwrap();
    assert!(!e.missing && e.last_opened_at.is_some());
    assert_eq!(e.repo_root_url, "https://relocated.example/svn/demo");
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

/// Paths the file manager was asked to show (the recording opener's `reveal` calls).
fn revealed(f: &Fixture) -> Vec<std::path::PathBuf> {
    f.opener
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter_map(|c| c.strip_prefix("reveal ").map(std::path::PathBuf::from))
        .collect()
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
    let root = parent.path().join("proj").canonicalize().unwrap();
    assert_eq!(revealed(&f), [root.clone(), root.join("readme.txt")]);
    for bad in [
        "../outside",
        "/etc",
        "a/../../b",
        "missing.txt",
        ".svn",
        "C:\\Windows",
        "readme.txt.",
    ] {
        assert!(
            commands::reveal_in_os(f.state(), id.clone(), bad.into())
                .await
                .is_err(),
            "{bad}"
        );
    }
    assert_eq!(revealed(&f).len(), 2);
}

/// Versioned symlinks (`svn:special`) are real links on Unix; the file manager would follow them.
#[cfg(unix)]
#[tokio::test]
async fn reveal_refuses_items_reached_through_a_symlink() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let id = checkout_into(&f, parent.path(), "proj").await;
    let wc = parent.path().join("proj");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("id_rsa"), b"secret").unwrap();
    std::fs::create_dir(wc.join("inside")).unwrap();
    std::fs::write(wc.join("inside/file.txt"), b"x").unwrap();
    symlink(outside.path(), wc.join("docs")).unwrap(); // directory link to outside
    symlink(outside.path().join("id_rsa"), wc.join("key")).unwrap(); // file link to outside
    symlink(wc.join("inside"), wc.join("in-link")).unwrap(); // link that stays inside
    symlink(wc.join("nowhere"), wc.join("dangling")).unwrap(); // dangling link
    for bad in [
        "docs",
        "docs/id_rsa",
        "key",
        "in-link",
        "in-link/file.txt",
        "dangling",
        "dangling/x",
    ] {
        let err = commands::reveal_in_os(f.state(), id.clone(), bad.into())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{bad}");
    }
    assert!(revealed(&f).is_empty(), "nothing was revealed");
    // the real folder the inside link points at is fine
    commands::reveal_in_os(f.state(), id.clone(), "inside/file.txt".into())
        .await
        .unwrap();
    assert_eq!(
        revealed(&f),
        [wc.canonicalize().unwrap().join("inside/file.txt")]
    );
}

/// The working copy root itself reached through a symlink (e.g. registered via a linked folder)
/// still reveals items inside it, by their real path.
#[cfg(unix)]
#[test]
fn reveal_target_resolves_a_linked_root_and_stays_inside_it() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("a.txt"), b"x").unwrap();
    symlink(&real, dir.path().join("link")).unwrap();
    let got = crate::os_integration::reveal_target(&dir.path().join("link"), "a.txt").unwrap();
    assert_eq!(got, real.canonicalize().unwrap().join("a.txt"));
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

#[tokio::test]
async fn an_unfinished_checkout_is_registered_as_resumable_only_when_its_working_copy_exists() {
    use crate::engine::{CheckoutPlan, EngineFacade, OpContext};

    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("partial");
    std::fs::create_dir(&root).unwrap();
    // The engine can read a working copy here (as after a checkout that stopped half-way)...
    f.engine
        .checkout(
            &CheckoutPlan {
                url: format!("{DEMO_REPO_URL}/trunk"),
                path: root.clone(),
                rev: None,
                depth: Depth::Infinity,
                sparse: None,
                format: WcFormat::V31,
                store_pristine: true,
                ignore_externals: false,
            },
            &OpContext::detached(),
        )
        .await
        .unwrap();
    // ...but without an administrative folder directly inside, nothing is registered.
    assert!(f.state().wcs.add_unfinished(&root).await.is_none());
    assert!(commands::wc_entries(f.state()).await.unwrap().is_empty());

    std::fs::create_dir(root.join(".svn")).unwrap();
    let wc = f.state().wcs.add_unfinished(&root).await.unwrap();
    let entries = commands::wc_entries(f.state()).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, wc.id);
    assert!(!entries[0].missing);
    // Registering again is idempotent.
    let again = f.state().wcs.add_unfinished(&root).await.unwrap();
    assert_eq!(again.id, wc.id);

    // A folder that is no working copy at all is left alone.
    let other = parent.path().join("other");
    std::fs::create_dir_all(other.join(".svn")).unwrap();
    assert!(f.state().wcs.add_unfinished(&other).await.is_none());
}

#[tokio::test]
async fn start_up_keeps_only_the_journal_of_checkouts_that_left_a_working_copy() {
    use crate::db::CheckoutJournal;

    let f = Fixture::new(Duration::ZERO).await;
    let parent = tempfile::tempdir().unwrap();
    // Killed before the working copy existed: only the user's folder, which stays as it is.
    let none = parent.path().join("never-started");
    std::fs::create_dir(&none).unwrap();
    std::fs::write(none.join("notes.txt"), "mine").unwrap();
    for root in [&none, &parent.path().join("gone")] {
        f.state()
            .db
            .checkout_journal_put(&CheckoutJournal {
                root_path: root.to_string_lossy().into_owned(),
                url: format!("{DEMO_REPO_URL}/trunk"),
                depth: "infinity".into(),
                ignore_externals: false,
            })
            .unwrap();
    }
    f.state().recover_checkouts().await;
    assert!(f.state().db.checkout_journal_list().unwrap().is_empty());
    assert!(commands::wc_entries(f.state()).await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(none.join("notes.txt")).unwrap(),
        "mine"
    );
}

#[tokio::test]
async fn engine_capabilities_come_from_the_engine() {
    let f = Fixture::new(Duration::ZERO).await;
    let caps = commands::engine_capabilities(f.state()).await.unwrap();
    // The fixture runs the demo engine; debug builds never show its banner.
    assert_eq!(
        caps,
        opensvn_ipc::EngineCapabilities {
            externals: true,
            demo: false,
        }
    );
}

#[test]
fn the_demo_engine_banner_is_shown_only_in_release_builds() {
    use crate::commands::visible_capabilities;
    let demo = opensvn_ipc::EngineCapabilities {
        externals: true,
        demo: true,
    };
    assert!(visible_capabilities(demo, true).demo);
    assert!(!visible_capabilities(demo, false).demo);
    let real = opensvn_ipc::EngineCapabilities {
        externals: false,
        demo: false,
    };
    assert!(!visible_capabilities(real, true).demo);
}
