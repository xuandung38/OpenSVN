#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::defaults::language_from_locale;
use super::*;
use opensvn_ipc::ErrorCode;
use opensvn_ipc::{DiffMode, ProxySetting, Theme};

fn service() -> SettingsService {
    SettingsService::load(AppDb::open_in_memory().unwrap(), Some("vi_VN.UTF-8")).unwrap()
}

fn patch(json: serde_json::Value) -> SettingsPatch {
    serde_json::from_value(json).unwrap()
}

fn meld() -> ExternalDiffTool {
    ExternalDiffTool {
        path: "/usr/bin/meld".into(),
        args: vec!["%l".into(), "%r".into()],
    }
}

#[test]
fn defaults_follow_doc_and_locale() {
    let s = service().get();
    assert_eq!(s.general_check_interval_min, 10);
    assert_eq!(s.network_max_connections, 4);
    assert_eq!(s.general_language, opensvn_ipc::Language::Vi);
    assert_eq!(s.diff_external_tool, None);
    assert_eq!(
        language_from_locale(Some("en_US.UTF-8")),
        opensvn_ipc::Language::En
    );
    assert_eq!(language_from_locale(None), opensvn_ipc::Language::En);
}

#[test]
fn patch_changes_only_present_keys_and_persists() {
    let db = AppDb::open_in_memory().unwrap();
    let svc = SettingsService::load(db.clone(), None).unwrap();
    let after = svc
        .apply_patch(&patch(serde_json::json!({
            "appearance.theme": "dark",
            "diff.mode": "unified",
        })))
        .unwrap();
    assert_eq!(after.appearance_theme, Theme::Dark);
    assert_eq!(after.diff_mode, DiffMode::Unified);
    assert_eq!(after.network_timeout_sec, 60);
    // a new service over the same database sees the stored values
    let reloaded = SettingsService::load(db, None).unwrap().get();
    assert_eq!(reloaded, after);
}

#[test]
fn invalid_patch_changes_nothing() {
    let svc = service();
    let before = svc.get();
    let err = svc
        .apply_patch(&patch(serde_json::json!({
            "appearance.theme": "dark",
            "network.maxConnections": 99,
        })))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
    assert_eq!(
        svc.get(),
        before,
        "valid keys of a rejected patch are not applied"
    );
    // wrong JSON type is rejected at deserialization of the effective value
    assert!(
        svc.apply_patch(&patch(serde_json::json!({"general.checkIntervalMin": 0})))
            .is_err()
    );
}

#[test]
fn a_patch_cannot_carry_program_launching_keys() {
    for key in PROGRAM_KEYS {
        for value in [
            serde_json::json!({"path": "/bin/sh", "args": ["-c", "id"]}),
            serde_json::Value::Null,
        ] {
            let err = serde_json::from_value::<SettingsPatch>(serde_json::json!({ key: value }))
                .unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{key}: {err}");
        }
    }
    assert_eq!(service().get().diff_external_tool, None);
}

#[test]
fn program_keys_cover_every_nullable_setting() {
    // Nullable settings are exactly the ones that hold a program.
    let nullable: std::collections::BTreeSet<String> = to_map(&defaults(None))
        .unwrap()
        .into_iter()
        .filter(|(_, v)| v.is_null())
        .map(|(k, _)| k)
        .collect();
    let listed: std::collections::BTreeSet<String> =
        PROGRAM_KEYS.iter().map(|k| (*k).to_owned()).collect();
    assert_eq!(nullable, listed);
}

#[test]
fn set_program_stores_and_clears() {
    let svc = service();
    let mut rx = svc.subscribe();
    let s = svc
        .set_program(ProgramKind::DiffTool, Some(meld()))
        .unwrap();
    assert_eq!(s.diff_external_tool, Some(meld()));
    assert!(rx.has_changed().unwrap());
    rx.mark_unchanged();
    let s = svc.set_program(ProgramKind::DiffTool, None).unwrap();
    assert_eq!(s.diff_external_tool, None);
    assert!(
        svc.set_program(
            ProgramKind::Editor,
            Some(ExternalDiffTool {
                path: "  ".into(),
                args: vec![]
            })
        )
        .is_err()
    );
}

#[test]
fn subscribers_are_notified_of_accepted_changes_only() {
    let svc = service();
    let mut rx = svc.subscribe();
    svc.apply_patch(&patch(serde_json::json!({"general.checkIntervalMin": 3})))
        .unwrap();
    assert!(rx.has_changed().unwrap());
    assert_eq!(rx.borrow_and_update().general_check_interval_min, 3);
    let _ = svc.apply_patch(&patch(serde_json::json!({"general.checkIntervalMin": 0})));
    assert!(!rx.has_changed().unwrap());
}

#[test]
fn proxy_url_with_credentials_is_refused() {
    let svc = service();
    let p =
        patch(serde_json::json!({"network.proxy": {"mode": "custom", "url": "http://u:p@h:1"}}));
    assert!(svc.apply_patch(&p).is_err());
    let ok = patch(serde_json::json!({"network.proxy": {"mode": "custom", "url": "http://h:1"}}));
    assert_eq!(
        svc.apply_patch(&ok).unwrap().network_proxy,
        ProxySetting::Custom {
            url: "http://h:1".into()
        }
    );
}

#[test]
fn corrupt_stored_value_falls_back_to_default() {
    let db = AppDb::open_in_memory().unwrap();
    db.settings_apply(&[
        ("network.maxConnections".into(), Some("500".into())),
        ("appearance.theme".into(), Some("\"dark\"".into())),
        ("unknown.key".into(), Some("1".into())),
        ("diff.mode".into(), Some("not json".into())),
    ])
    .unwrap();
    let s = SettingsService::load(db, None).unwrap().get();
    assert_eq!(s.network_max_connections, 4);
    assert_eq!(s.appearance_theme, Theme::Dark);
    assert_eq!(s.diff_mode, DiffMode::Split);
}
