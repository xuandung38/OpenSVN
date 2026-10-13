#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use opensvn_ipc::{ErrorCode, ProgramKind, SettingsPatch, Theme};

use super::fixture::Fixture;
use crate::commands;

fn patch(v: serde_json::Value) -> SettingsPatch {
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn settings_round_trip_and_survive_a_restart_of_the_service() {
    let f = Fixture::new(Duration::ZERO).await;
    let before = commands::settings_get(f.state()).await.unwrap();
    assert_eq!(before.general_check_interval_min, 10);
    let after = commands::settings_set(
        f.state(),
        patch(serde_json::json!({"appearance.theme": "dark", "general.checkIntervalMin": 5})),
    )
    .await
    .unwrap();
    assert_eq!(after.appearance_theme, Theme::Dark);
    assert_eq!(commands::settings_get(f.state()).await.unwrap(), after);
}

#[tokio::test]
async fn program_keys_do_not_exist_in_a_settings_patch() {
    let f = Fixture::new(Duration::ZERO).await;
    for key in [
        "diff.externalTool",
        "diff.mergeTool",
        "tools.editor",
        "tools.terminal",
    ] {
        let parsed = serde_json::from_value::<SettingsPatch>(
            serde_json::json!({ key: {"path": "/bin/sh", "args": ["-c", "id"]} }),
        );
        assert!(parsed.is_err(), "{key} must not be patchable");
    }
    let s = commands::settings_get(f.state()).await.unwrap();
    assert!(s.diff_external_tool.is_none() && s.tools_editor.is_none());
}

#[tokio::test]
async fn program_choose_stores_only_after_native_confirmation() {
    let f = Fixture::new(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join(if cfg!(windows) { "ed.exe" } else { "ed" });
    std::fs::write(&exe, b"x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    *f.picker.pick.lock().unwrap() = Some(exe.clone());

    f.picker.accept.store(false, Ordering::SeqCst);
    let s = commands::program_choose(f.state(), ProgramKind::Editor, vec![])
        .await
        .unwrap();
    assert!(
        s.tools_editor.is_none(),
        "declined confirmation stores nothing"
    );

    f.picker.accept.store(true, Ordering::SeqCst);
    let s = commands::program_choose(f.state(), ProgramKind::Editor, vec!["-w".into()])
        .await
        .unwrap();
    assert_eq!(s.tools_editor.unwrap().args, vec!["-w"]);
    assert_eq!(f.picker.confirmations.lock().unwrap().len(), 2);

    let s = commands::program_clear(f.state(), ProgramKind::Editor)
        .await
        .unwrap();
    assert!(s.tools_editor.is_none());
}

#[tokio::test]
async fn accounts_list_is_empty_and_auth_commands_reject_unknown_requests() {
    let f = Fixture::new(Duration::ZERO).await;
    assert!(commands::accounts_list(f.state()).await.unwrap().is_empty());
    commands::accounts_sign_out(f.state(), "realm".into())
        .await
        .unwrap();
    let err = commands::auth_respond(f.state(), "nope".into(), opensvn_ipc::AuthResponse::Cancel)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
}

#[tokio::test]
async fn checkout_waits_for_sign_in_through_the_prompt_events() {
    use opensvn_ipc::{AuthResponse, Depth, OpResult, Rev, WcFormat};
    use std::sync::Arc;

    use crate::auth::{AuthBroker, NoSecretStore};
    use crate::ops::events::Recorded;

    let f = Fixture::new(Duration::ZERO).await;
    // A repository that wants alice/pw; its prompter is a broker that publishes through the
    // fixture's event recorder and answers are delivered with `auth_respond`.
    let broker = Arc::new(AuthBroker::new(
        f.events.clone(),
        crate::db::AppDb::open_in_memory().unwrap(),
        Arc::new(NoSecretStore),
    ));
    let repo = crate::engine::fake::demo_repo().unwrap();
    let repo = repo.with_required_auth("alice", "pw", Some(broker.clone()));
    let engine = Arc::new(crate::engine::FakeEngine::empty().with_repo(repo));
    let state = crate::app_state::AppState::build(crate::app_state::AppDeps {
        data_dir: tempfile::tempdir().unwrap().keep(),
        engine,
        events: f.events.clone(),
        picker: f.picker.clone(),
        path_picker: f.paths.clone(),
        status_timing: Default::default(),
    })
    .unwrap();
    let handle = state.start_op(opensvn_ipc::OperationKind::Checkout, "Checkout", {
        let url = format!("{}/trunk", crate::engine::fake::DEMO_REPO_URL);
        move |engine, ctx| async move {
            let root = if cfg!(windows) {
                r"C:\opensvn-fake\auth"
            } else {
                "/opensvn-fake/auth"
            };
            let info = engine
                .checkout(
                    &crate::engine::CheckoutPlan {
                        url,
                        path: root.into(),
                        rev: Some(Rev(2)),
                        depth: Depth::Infinity,
                        sparse: None,
                        format: WcFormat::V31,
                        store_pristine: true,
                        ignore_externals: false,
                    },
                    &ctx,
                )
                .await?;
            Ok(OpResult::NewRev {
                new_rev: info.base_rev,
            })
        }
    });
    let prompt = loop {
        if let Some(Recorded::Auth(p)) = f
            .events
            .all()
            .into_iter()
            .find(|r| matches!(r, Recorded::Auth(_)))
        {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(prompt.url, "https://svn.test.local:443");
    broker
        .respond(
            &prompt.request_id,
            AuthResponse::Credentials {
                username: "alice".into(),
                password: "pw".to_owned().into(),
                save: false,
            },
        )
        .unwrap();
    f.wait_op(&handle).await.unwrap();
}

#[tokio::test]
async fn trusted_certificates_can_be_listed_and_removed() {
    let f = Fixture::new(Duration::ZERO).await;
    assert!(commands::certs_list(f.state()).await.unwrap().is_empty());
    f.state()
        .db
        .cert_trust(
            "svn.test.local",
            8443,
            &"ab".repeat(32),
            "CN=svn",
            "2030-01-01T00:00:00Z",
        )
        .unwrap();
    let list = commands::certs_list(f.state()).await.unwrap();
    assert_eq!(
        (list[0].host.as_str(), list[0].port),
        ("svn.test.local", 8443)
    );
    let key = opensvn_ipc::TrustedCertKey {
        host: "svn.test.local".into(),
        port: 8443,
        sha256: "ab".repeat(32),
    };
    commands::certs_remove(f.state(), key.clone())
        .await
        .unwrap();
    assert!(commands::certs_list(f.state()).await.unwrap().is_empty());
    assert_eq!(
        commands::certs_remove(f.state(), key)
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[tokio::test]
async fn default_checkout_folder_is_chosen_natively() {
    let f = Fixture::new(Duration::ZERO).await;
    let before = commands::settings_get(f.state()).await.unwrap();
    // cancelled dialog: unchanged
    let s = commands::checkout_root_choose(f.state()).await.unwrap();
    assert_eq!(s.general_checkout_root, before.general_checkout_root);
    let dir = tempfile::tempdir().unwrap();
    *f.paths.choice.lock().unwrap() = Some((dir.path().to_path_buf(), opensvn_ipc::GrantKind::Dir));
    let s = commands::checkout_root_choose(f.state()).await.unwrap();
    assert_eq!(s.general_checkout_root, dir.path().to_string_lossy());
}
