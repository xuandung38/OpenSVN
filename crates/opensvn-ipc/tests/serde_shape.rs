#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opensvn_ipc::*;
use serde_json::json;

#[test]
fn diff_target_is_tagged_by_kind() {
    let t = DiffTarget::WcVsRev {
        wc_id: WcId("w1".into()),
        path: "a/b".into(),
        rev: Rev(7),
    };
    let v = serde_json::to_value(&t).unwrap();
    assert_eq!(
        v,
        json!({"kind":"wcVsRev","wcId":"w1","path":"a/b","rev":7})
    );
    assert_eq!(serde_json::from_value::<DiffTarget>(v).unwrap(), t);
}

#[test]
fn layout_is_tagged_by_mode() {
    assert_eq!(
        serde_json::to_value(Layout::None).unwrap(),
        json!({"mode":"none"})
    );
    let custom = Layout::Custom {
        trunk: "/{project}/trunk".into(),
        branches: vec![],
        tags: vec![],
    };
    assert_eq!(
        serde_json::to_value(&custom).unwrap(),
        json!({"mode":"custom","trunk":"/{project}/trunk","branches":[],"tags":[]})
    );
}

#[test]
fn union_targets_use_kind_tag() {
    let l = LogTarget::Wc {
        wc_id: WcId("w".into()),
        path: "p".into(),
    };
    assert_eq!(
        serde_json::to_value(&l).unwrap(),
        json!({"kind":"wc","wcId":"w","path":"p"})
    );
    let k = LockTargets::Url {
        urls: vec!["https://svn.test.local/r".into()],
    };
    assert_eq!(
        serde_json::to_value(&k).unwrap(),
        json!({"kind":"url","urls":["https://svn.test.local/r"]})
    );
}

#[test]
fn string_unions_match_doc() {
    assert_eq!(
        serde_json::to_value(OperationKind::StatusRemote).unwrap(),
        json!("status-remote")
    );
    assert_eq!(serde_json::to_value(ChangeAction::M).unwrap(), json!("M"));
    assert_eq!(serde_json::to_value(Eol::Crlf).unwrap(), json!("CRLF"));
    assert_eq!(
        serde_json::to_value(IgnoreScope::SvnIgnore).unwrap(),
        json!("svn:ignore")
    );
    assert_eq!(
        serde_json::to_value(NodeStatus::Unversioned).unwrap(),
        json!("unversioned")
    );
}

#[test]
fn wc_format_accepts_only_31_and_32() {
    assert_eq!(
        serde_json::from_value::<WcFormat>(json!(31)).unwrap(),
        WcFormat::V31
    );
    assert_eq!(serde_json::to_value(WcFormat::V32).unwrap(), json!(32));
    assert!(serde_json::from_value::<WcFormat>(json!(30)).is_err());
}

#[test]
fn rev_is_a_plain_number() {
    assert_eq!(
        serde_json::to_value(Rev(9_007_199_254_740_991)).unwrap(),
        json!(9_007_199_254_740_991u64)
    );
}

#[test]
fn settings_patch_uses_dotted_keys_and_omits_absent() {
    let patch = SettingsPatch {
        network_timeout_sec: Some(30),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&patch).unwrap(),
        json!({"network.timeoutSec":30})
    );
    let back: SettingsPatch = serde_json::from_value(json!({"appearance.theme":"dark"})).unwrap();
    assert_eq!(back.appearance_theme, Some(Theme::Dark));
}

#[test]
fn proxy_setting_shape() {
    assert_eq!(
        serde_json::to_value(ProxySetting::System).unwrap(),
        json!({"mode":"system"})
    );
}

#[test]
fn password_is_redacted_in_debug_but_plain_on_the_wire() {
    let r: AuthResponse = serde_json::from_value(
        json!({"kind":"credentials","username":"alice","password":"hunter2","save":true}),
    )
    .unwrap();
    let dbg = format!("{r:?}");
    assert!(!dbg.contains("hunter2"), "{dbg}");
    assert!(dbg.contains("alice"));
    match &r {
        AuthResponse::Credentials { password, .. } => assert_eq!(password.expose(), "hunter2"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        serde_json::to_value(&r).unwrap()["password"],
        json!("hunter2")
    );
}
