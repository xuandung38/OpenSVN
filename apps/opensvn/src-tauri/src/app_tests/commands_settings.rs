#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use opensvn_ipc::{ProgramKind, SettingsPatch, Theme};

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
async fn accounts_list_is_empty_and_later_service_commands_are_stubs() {
    let f = Fixture::new(Duration::ZERO).await;
    assert!(commands::accounts_list(f.state()).await.unwrap().is_empty());
    let err = commands::accounts_sign_out("realm".into())
        .await
        .unwrap_err();
    assert_eq!(err.message, "not implemented");
}

#[tokio::test]
async fn diff_external_needs_a_configured_tool_and_launches_it_on_read_only_copies() {
    let f = Fixture::new(Duration::ZERO).await;
    let wc = f.add_demo_wc().await;
    let target = || opensvn_ipc::DiffTarget::Wc {
        wc_id: wc.id.clone(),
        path: "README.md".into(),
    };

    // Not configured: refused with a pointer to Settings, nothing launched.
    let err = commands::open_external_diff(&f.state(), target())
        .await
        .unwrap_err();
    assert_eq!(err.code, opensvn_ipc::ErrorCode::InvalidArgument);
    assert_eq!(
        err.actions,
        Some(vec![opensvn_ipc::ErrorAction::OpenSettings])
    );
    assert!(f.launcher.launched.lock().unwrap().is_empty());

    let dir = tempfile::tempdir().unwrap();
    let exe = dir
        .path()
        .join(if cfg!(windows) { "meld.exe" } else { "meld" });
    std::fs::write(&exe, b"x").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    *f.picker.pick.lock().unwrap() = Some(exe.clone());
    f.picker.accept.store(true, Ordering::SeqCst);
    commands::program_choose(
        f.state(),
        ProgramKind::DiffTool,
        vec!["--label=old".into(), "{left}".into(), "{right}".into()],
    )
    .await
    .unwrap();

    commands::open_external_diff(&f.state(), target())
        .await
        .unwrap();
    let launched = f.launcher.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 1);
    let (program, args) = &launched[0];
    assert_eq!(program, &std::fs::canonicalize(&exe).unwrap());
    assert_eq!(args.len(), 3);
    assert_eq!(args[0], "--label=old");
    let (left, right) = (
        std::path::Path::new(&args[1]),
        std::path::Path::new(&args[2]),
    );
    assert!(left.ends_with("left.md") && right.ends_with("right.md"));
    assert_eq!(left.parent(), right.parent());
    assert!(left.starts_with(&f.state().data_dir));
    assert!(
        std::fs::read_to_string(left)
            .unwrap()
            .contains("cargo build`")
    );
    assert!(
        std::fs::read_to_string(right)
            .unwrap()
            .contains("--release")
    );
    assert!(std::fs::metadata(right).unwrap().permissions().readonly());

    // Binary files are refused.
    let err = commands::open_external_diff(
        &f.state(),
        opensvn_ipc::DiffTarget::Rev {
            url: format!(
                "{}/trunk/assets/logo.png",
                crate::engine::fake::DEMO_REPO_URL
            ),
            rev: opensvn_ipc::Rev(2),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, opensvn_ipc::ErrorCode::InvalidArgument);
    assert_eq!(f.launcher.launched.lock().unwrap().len(), 1);
}
