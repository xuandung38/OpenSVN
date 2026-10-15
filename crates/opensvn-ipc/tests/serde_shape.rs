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
fn checkout_root_is_readable_but_not_patchable() {
    let s = serde_json::to_value(sample_settings()).unwrap();
    assert_eq!(s["general.checkoutRoot"], json!("~/svn"));
    let err = serde_json::from_value::<SettingsPatch>(json!({"general.checkoutRoot": "/etc"}))
        .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn checkout_request_and_result_shapes() {
    let req: CheckoutRequest = serde_json::from_value(json!({
        "url": "https://svn.test.local/r/trunk", "dest": "g1", "destName": "proj", "rev": null,
        "depth": "infinity", "sparse": null, "format": 31, "storePristine": true,
        "ignoreExternals": false
    }))
    .unwrap();
    assert_eq!(req.dest_name.as_deref(), Some("proj"));
    assert_eq!(
        serde_json::to_value(OpResult::CheckedOut {
            wc_id: Some(WcId("w".into())),
            new_rev: Rev(3)
        })
        .unwrap(),
        json!({"kind": "checkedOut", "wcId": "w", "newRev": 3})
    );
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

#[test]
fn program_settings_are_readable_but_not_patchable() {
    let mut settings = serde_json::to_value(sample_settings()).unwrap();
    settings["tools.editor"] = json!({"path": "/usr/bin/code", "args": []});
    let back: Settings = serde_json::from_value(settings).unwrap();
    assert!(back.tools_editor.is_some() && back.diff_merge_tool.is_none());
    for key in [
        "diff.externalTool",
        "diff.mergeTool",
        "tools.editor",
        "tools.terminal",
        "diff.toolsByExtension",
    ] {
        let err = serde_json::from_value::<SettingsPatch>(json!({ key: null })).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{key}: {err}");
    }
    assert_eq!(
        serde_json::to_value(ProgramKind::MergeTool).unwrap(),
        json!("merge-tool")
    );
}

#[test]
fn operation_kinds_for_local_edits_and_downloads() {
    for (kind, wire) in [
        (OperationKind::StatusRefresh, "status-refresh"),
        (OperationKind::Add, "add"),
        (OperationKind::Delete, "delete"),
        (OperationKind::Move, "move"),
        (OperationKind::Copy, "copy"),
        (OperationKind::Save, "save"),
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(wire));
    }
}

fn sample_settings() -> Settings {
    Settings {
        general_checkout_root: "~/svn".into(),
        general_check_interval_min: 10,
        general_language: Language::En,
        appearance_theme: Theme::System,
        diff_mode: DiffMode::Split,
        diff_ignore_whitespace: false,
        network_max_connections: 4,
        network_timeout_sec: 60,
        network_reconnect_wait_min: 10,
        network_proxy: ProxySetting::System,
        network_negotiate: true,
        wc_default_format: WcFormat::V31,
        wc_read_subversion_config: true,
        cache_max_size_gb: 2,
        integrations_web_revision_url: "https://{server}/r{rev}".into(),
        integrations_web_file_url: "https://{server}/{path}".into(),
        diff_external_tool: None,
        diff_merge_tool: None,
        tools_editor: None,
        tools_terminal: None,
        diff_tools_by_extension: None,
    }
}

#[test]
fn path_grants_are_opaque_ids_with_display_names() {
    let dropped = DroppedPaths {
        grants: vec![PathGrantInfo {
            id: PathGrantId("g1".into()),
            display_name: "project".into(),
            kind: GrantKind::Dir,
        }],
        position: DropPosition { x: 10, y: 20 },
    };
    assert_eq!(
        serde_json::to_value(&dropped).unwrap(),
        json!({"grants": [{"id": "g1", "displayName": "project", "kind": "dir"}], "position": {"x": 10, "y": 20}})
    );
    assert_eq!(
        serde_json::to_value(PickRequest::SaveFile {
            suggested_name: "a.txt".into(),
            title: None
        })
        .unwrap(),
        json!({"kind": "saveFile", "suggestedName": "a.txt", "title": null})
    );
    assert_eq!(
        serde_json::to_value(ErrorCode::InvalidArgument).unwrap(),
        json!("invalid-argument")
    );
}

#[test]
fn auth_prompt_carries_reason_offered_methods_and_structured_cert_failure() {
    let prompt = AuthPrompt {
        request_id: "r".into(),
        kind: AuthPromptKind::Credentials,
        realm: "realm".into(),
        url: "https://svn.test.local:443".into(),
        cert: None,
        reason: Some(AuthPromptReason::NegotiateFailed),
        attempt: 1,
        offered: vec![AuthMethodKind::Negotiate, AuthMethodKind::Basic],
    };
    let v = serde_json::to_value(&prompt).unwrap();
    assert_eq!(v["reason"], "negotiate-failed");
    assert_eq!(v["offered"], json!(["negotiate", "basic"]));
    assert_eq!(
        serde_json::to_value(CertFailureInfo::Changed {
            old_sha256: "aa".into(),
            new_sha256: "bb".into()
        })
        .unwrap(),
        json!({"changed": {"oldSha256": "aa", "newSha256": "bb"}})
    );
    assert_eq!(
        serde_json::to_value(CertFailureInfo::SelfSigned).unwrap(),
        json!("self-signed")
    );
    assert_eq!(
        serde_json::to_value(CertFailureInfo::Other("x".into())).unwrap(),
        json!({"other": "x"})
    );
    let sso: AuthResponse = serde_json::from_value(json!({"kind": "negotiate"})).unwrap();
    assert!(matches!(sso, AuthResponse::Negotiate));
}

#[test]
fn certificate_decisions_use_camel_case_on_the_wire() {
    for (d, wire) in [
        (CertDecision::Reject, "reject"),
        (CertDecision::AcceptOnce, "acceptOnce"),
        (CertDecision::AcceptAlways, "acceptAlways"),
        (CertDecision::ReplacePin, "replacePin"),
    ] {
        let r = AuthResponse::Certificate { decision: d };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, json!({"kind":"certificate","decision":wire}));
        assert_eq!(serde_json::from_value::<AuthResponse>(v).unwrap(), r);
    }
}

#[test]
fn file_diff_flags_too_complex_in_camel_case() {
    let d = FileDiff {
        path: "big.txt".into(),
        binary: false,
        mime: None,
        left: None,
        right: None,
        hunks: None,
        prop_changes: vec![],
        image: None,
        too_large: None,
        too_complex: Some(true),
        office: None,
    };
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["tooComplex"], json!(true));
    assert_eq!(v["left"], json!(null));
    assert_eq!(serde_json::from_value::<FileDiff>(v).unwrap(), d);
}

#[test]
fn engine_capabilities_shape() {
    let caps = EngineCapabilities {
        externals: false,
        demo: true,
    };
    let v = serde_json::to_value(caps).unwrap();
    assert_eq!(v, json!({"externals": false, "demo": true}));
    assert_eq!(
        serde_json::from_value::<EngineCapabilities>(v).unwrap(),
        caps
    );
}
