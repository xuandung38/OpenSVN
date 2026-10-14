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
fn out_of_range_stored_numbers_are_clamped_on_load() {
    let db = AppDb::open_in_memory().unwrap();
    db.settings_apply(&[
        // Accepted by older UIs (1..=16, 0 = off, 1 s, ...), refused by the validator now.
        ("network.maxConnections".into(), Some("16".into())),
        ("general.checkIntervalMin".into(), Some("0".into())),
        ("network.timeoutSec".into(), Some("1".into())),
        ("cache.maxSizeGb".into(), Some("9000".into())),
    ])
    .unwrap();
    let s = SettingsService::load(db, None).unwrap().get();
    assert_eq!(s.network_max_connections, 8);
    assert_eq!(s.general_check_interval_min, 1);
    assert_eq!(s.network_timeout_sec, 5);
    assert_eq!(s.cache_max_size_gb, 4096);
}

#[test]
fn every_ranged_key_is_a_numeric_setting() {
    let map = to_map(&defaults(None)).unwrap();
    for (key, r) in SETTING_RANGES {
        assert!(map.get(key).is_some_and(Value::is_u64), "{key}");
        assert!(r.min <= r.max, "{key}");
    }
    assert_eq!(setting_ranges().len(), SETTING_RANGES.len());
}

#[test]
fn corrupt_stored_value_falls_back_to_default() {
    let db = AppDb::open_in_memory().unwrap();
    db.settings_apply(&[
        ("network.maxConnections".into(), Some("\"many\"".into())),
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

#[test]
fn checkout_root_changes_only_through_the_picker_path() {
    let err = serde_json::from_value::<SettingsPatch>(serde_json::json!({
        "general.checkoutRoot": "/etc"
    }))
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
    let svc = service();
    assert_eq!(svc.get().general_checkout_root, "~/svn");
    let s = svc.set_checkout_root("/home/me/work").unwrap();
    assert_eq!(s.general_checkout_root, "/home/me/work");
    assert!(svc.set_checkout_root("  ").is_err());
}

/// The UI validates with `SETTING_RANGES` from the generated bindings: they must be exactly the
/// validator's ranges (a stale `bindings.ts` fails here; `pnpm gen:ipc` regenerates it).
#[test]
fn the_ui_bindings_carry_the_validator_ranges() {
    let ts = include_str!("../../../src/ipc/bindings.ts");
    let json = ts
        .split("export const SETTING_RANGES = ")
        .nth(1)
        .and_then(|rest| rest.split(" as const;").next())
        .expect("SETTING_RANGES in bindings.ts");
    let ui: std::collections::BTreeMap<String, opensvn_ipc::SettingRange> =
        serde_json::from_str(json).unwrap();
    let rust: std::collections::BTreeMap<String, opensvn_ipc::SettingRange> = SETTING_RANGES
        .into_iter()
        .map(|(k, r)| (k.to_owned(), r))
        .collect();
    assert_eq!(ui, rust);
}
