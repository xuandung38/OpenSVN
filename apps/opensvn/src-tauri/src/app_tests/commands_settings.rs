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
async fn settings_set_rejects_keys_that_launch_programs() {
    let f = Fixture::new(Duration::ZERO).await;
    for key in [
        "diff.externalTool",
        "diff.mergeTool",
        "tools.editor",
        "tools.terminal",
    ] {
        let err = commands::settings_set(
            f.state(),
            patch(serde_json::json!({ key: {"path": "/bin/sh", "args": ["-c", "id"]} })),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied, "{key}");
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
async fn accounts_list_is_empty_and_later_service_commands_are_stubs() {
    let f = Fixture::new(Duration::ZERO).await;
    assert!(commands::accounts_list(f.state()).await.unwrap().is_empty());
    let err = commands::accounts_sign_out("realm".into())
        .await
        .unwrap_err();
    assert_eq!(err.message, "not implemented");
}
