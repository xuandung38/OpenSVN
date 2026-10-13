#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opensvn_ipc::{ErrorCode, GrantKind, PickRequest};

use super::fixture::Fixture;
use crate::commands;
use crate::ops::events::Recorded;

#[tokio::test]
async fn path_pick_returns_an_opaque_grant_and_none_on_cancel() {
    let f = Fixture::new(Duration::ZERO).await;
    let req = PickRequest::Folder { title: None };
    assert!(
        commands::path_pick(f.state(), req.clone())
            .await
            .unwrap()
            .is_none()
    );

    let secret = PathBuf::from("/home/someone/secret-project");
    *f.paths.choice.lock().unwrap() = Some((secret.clone(), GrantKind::Dir));
    let grant = commands::path_pick(f.state(), req).await.unwrap().unwrap();
    assert_eq!(grant.display_name, "secret-project");
    assert!(
        !serde_json::to_string(&grant).unwrap().contains("someone"),
        "no real path leaks"
    );
    // the grant works once for a folder and then is gone
    assert_eq!(f.state().redeem_dir(&grant.id).unwrap(), secret);
    assert_eq!(
        f.state().redeem_dir(&grant.id).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[tokio::test]
async fn native_drop_becomes_grants_and_never_exposes_paths() {
    let f = Fixture::new(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"x").unwrap();
    f.state()
        .handle_drop(&[dir.path().to_path_buf(), file.clone()], 120.4, 33.6);
    let dropped = f
        .events
        .all()
        .into_iter()
        .find_map(|r| {
            if let Recorded::Dropped(d) = r {
                Some(d)
            } else {
                None
            }
        })
        .expect("fs://dropped");
    assert_eq!((dropped.position.x, dropped.position.y), (120, 34));
    assert_eq!(dropped.grants.len(), 2);
    assert_eq!(dropped.grants[0].kind, GrantKind::Dir);
    assert_eq!(dropped.grants[1].kind, GrantKind::File);
    let wire = serde_json::to_string(&dropped).unwrap();
    assert!(
        !wire.contains(&*dir.path().to_string_lossy()),
        "dropped paths stay in Rust: {wire}"
    );
    assert_eq!(f.state().redeem_file(&dropped.grants[1].id).unwrap(), file);
    // nothing is emitted for an empty drop
    let before = f.events.all().len();
    f.state().handle_drop(&[], 0.0, 0.0);
    assert_eq!(f.events.all().len(), before);
}

#[tokio::test]
async fn remove_and_re_add_reuse_the_same_actor() {
    let f = Fixture::new(Duration::ZERO).await;
    let first = f.add_demo_wc().await;
    let actor1 = f.state().wcs.resolve(&first.id).unwrap().actor;
    commands::wc_remove(f.state(), first.id).await.unwrap();
    let second = f.add_demo_wc().await;
    let actor2 = f.state().wcs.resolve(&second.id).unwrap().actor;
    assert!(
        Arc::ptr_eq(&actor1, &actor2),
        "one writer per working copy folder"
    );
}

#[tokio::test]
async fn upload_in_a_remote_edit_needs_a_file_grant() {
    use opensvn_ipc::{OpResult, RemoteEditRequest, RemoteOp, Rev};
    let f = Fixture::new(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("new.txt");
    std::fs::write(&local, b"uploaded\n").unwrap();
    let url = format!("{}/trunk/new.txt", crate::engine::fake::DEMO_REPO_URL);
    let edit = |source| RemoteEditRequest {
        ops: vec![RemoteOp::Upload {
            url: url.clone(),
            source,
        }],
        message: "Upload".into(),
    };
    // a folder grant or a spent grant is refused before anything is sent
    let err = commands::remote_edit(f.state(), edit(f.grant_dir(dir.path())))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    let h = commands::remote_edit(f.state(), edit(f.grant_file(&local)))
        .await
        .unwrap();
    assert_eq!(
        f.wait_op(&h).await.unwrap(),
        OpResult::NewRev { new_rev: Rev(11) }
    );
    let text = commands::repo_cat(f.state(), url, None).await.unwrap();
    assert_eq!(text.content, "uploaded\n");
}
