//! WebView and IPC hardening: CSP, capability allowlist, asset scope, no general-purpose plugins.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;
use tauri::Manager;
use tauri::test::{INVOKE_KEY, MockRuntime, get_ipc_response};
use tauri::webview::InvokeRequest;
use tauri::{
    WebviewWindowBuilder,
    ipc::{CallbackFn, InvokeBody},
};

use super::fixture::Fixture;
use crate::{COMMAND_NAMES, builder};

const CONF: &str = include_str!("../../tauri.conf.json");
const CAPABILITY: &str = include_str!("../../capabilities/default.json");
const CARGO: &str = include_str!("../../Cargo.toml");

fn conf() -> Value {
    serde_json::from_str(CONF).unwrap()
}

/// `directive -> sources` of a CSP string.
fn parse_csp(csp: &str) -> BTreeMap<String, Vec<String>> {
    csp.split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| {
            let mut parts = d.split_whitespace();
            let name = parts.next().unwrap().to_owned();
            (name, parts.map(str::to_owned).collect())
        })
        .collect()
}

fn sources<'a>(csp: &'a BTreeMap<String, Vec<String>>, d: &str) -> Vec<&'a str> {
    csp.get(d)
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

#[test]
fn production_csp_is_strict() {
    let conf = conf();
    let csp = parse_csp(conf["app"]["security"]["csp"].as_str().unwrap());
    assert_eq!(sources(&csp, "default-src"), ["'self'"]);
    assert_eq!(sources(&csp, "script-src"), ["'self'"]);
    assert_eq!(sources(&csp, "style-src"), ["'self'"]);
    assert_eq!(
        sources(&csp, "connect-src"),
        ["ipc:", "http://ipc.localhost"]
    );
    assert_eq!(sources(&csp, "object-src"), ["'none'"]);
    assert_eq!(sources(&csp, "base-uri"), ["'none'"]);
    assert_eq!(sources(&csp, "form-action"), ["'none'"]);
    assert_eq!(sources(&csp, "frame-ancestors"), ["'none'"]);
    let img = sources(&csp, "img-src");
    for required in ["'self'", "asset:", "blob:", "data:"] {
        assert!(img.contains(&required), "img-src lacks {required}");
    }
    // `http://asset.localhost` is how Windows serves the asset protocol; nothing else is remote.
    assert!(
        img.iter()
            .all(|s| !s.starts_with("http") || *s == "http://asset.localhost")
    );
    let raw = conf["app"]["security"]["csp"].as_str().unwrap();
    assert!(!raw.contains("unsafe-inline") && !raw.contains("unsafe-eval") && !raw.contains('*'));
}

#[test]
fn dev_csp_only_adds_the_vite_dev_server() {
    let conf = conf();
    let dev = parse_csp(conf["app"]["security"]["devCsp"].as_str().unwrap());
    assert!(
        !conf["app"]["security"]["devCsp"]
            .as_str()
            .unwrap()
            .contains("unsafe-eval")
    );
    assert_eq!(sources(&dev, "script-src"), ["'self'"]);
    let connect = sources(&dev, "connect-src");
    for s in &connect {
        assert!(
            matches!(
                *s,
                "ipc:" | "http://ipc.localhost" | "http://127.0.0.1:1420" | "ws://127.0.0.1:1420"
            ),
            "unexpected connect-src {s}"
        );
    }
    assert_eq!(sources(&dev, "default-src"), ["'self'"]);
}

#[test]
fn asset_protocol_is_limited_to_the_app_temp_folder() {
    let conf = conf();
    let asset = &conf["app"]["security"]["assetProtocol"];
    assert_eq!(asset["enable"], true);
    assert_eq!(asset["scope"], serde_json::json!(["$TEMP/opensvn/**"]));
    assert!(
        conf["app"]["security"]["freezePrototype"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn asset_scope_rejects_paths_outside_the_app_temp_folder() {
    // The runtime scope is built from tauri.conf.json by the real context.
    let app = tauri::test::mock_builder()
        .build(crate::app_context())
        .unwrap();
    let scope = app.asset_protocol_scope();
    let tmp = std::env::temp_dir();
    assert!(scope.is_allowed(tmp.join("opensvn").join("diff").join("left.png")));
    assert!(!scope.is_allowed(tmp.join("other.png")));
    assert!(!scope.is_allowed(tmp.join("opensvn-evil").join("x.png")));
    assert!(!scope.is_allowed("/etc/passwd"));
    #[cfg(unix)]
    assert!(!scope.is_allowed(tmp.join("opensvn").join("..").join("secret")));
}

#[test]
fn capability_is_main_window_only_with_exact_command_allowlist() {
    let cap: Value = serde_json::from_str(CAPABILITY).unwrap();
    assert_eq!(cap["windows"], serde_json::json!(["main"]));
    assert!(cap.get("remote").is_none() && cap.get("webviews").is_none());
    let all = cap["permissions"].as_array().unwrap();
    // The only scoped permission: opening https links in the browser.
    let scoped: Vec<&Value> = all.iter().filter(|p| p.is_object()).collect();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0]["identifier"], "opener:allow-open-url");
    assert_eq!(
        scoped[0]["allow"],
        serde_json::json!([{"url": "https://*"}])
    );
    assert!(scoped[0].get("deny").is_none());
    let perms: Vec<&str> = all.iter().filter_map(|p| p.as_str()).collect();
    for p in &perms {
        assert!(
            p.starts_with("allow-")
                || matches!(*p, "core:event:allow-listen" | "core:event:allow-unlisten"),
            "unexpected permission {p}"
        );
        assert!(
            !p.starts_with("allow-")
                || COMMAND_NAMES
                    .iter()
                    .any(|c| format!("allow-{}", c.replace('_', "-")) == *p),
            "{p} is not an app command"
        );
    }
    // every command is allowed explicitly, no wildcard / default sets
    assert_eq!(perms.len(), COMMAND_NAMES.len() + 2);
}

#[test]
fn no_general_purpose_plugins_are_linked() {
    for plugin in [
        "tauri-plugin-shell",
        "tauri-plugin-fs",
        "tauri-plugin-http",
        "tauri-plugin-dialog",
        "tauri-plugin-process",
    ] {
        assert!(!CARGO.contains(plugin), "{plugin} must not be a dependency");
    }
}

/// App with the real ACL (from `capabilities/` and the generated permissions) and a `main` window.
fn ipc_app(f: &Fixture) -> (tauri::App<MockRuntime>, tauri::WebviewWindow<MockRuntime>) {
    let reg = builder::<MockRuntime>();
    let app = tauri::test::mock_builder()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(reg.invoke_handler())
        .build(crate::app_context())
        .unwrap();
    let state = crate::app_state::AppState::build(crate::app_state::AppDeps {
        data_dir: tempfile::tempdir().unwrap().keep(),
        engine: f.engine.clone(),
        events: std::sync::Arc::new(crate::ops::events::RecordingEvents::default()),
        picker: f.picker.clone(),
    })
    .unwrap();
    app.manage(state);
    let main = WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    (app, main)
}

fn invoke(
    window: &tauri::WebviewWindow<MockRuntime>,
    cmd: &str,
    body: Value,
) -> Result<Value, Value> {
    let res = get_ipc_response(
        window,
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    );
    res.map(|b| b.deserialize::<Value>().unwrap())
}

#[tokio::test]
async fn ipc_allows_listed_commands_and_rejects_everything_else() {
    let f = Fixture::new(Duration::ZERO).await;
    let (_app, main) = ipc_app(&f);

    let settings = invoke(&main, "settings_get", serde_json::json!({})).unwrap();
    assert_eq!(settings["general.checkIntervalMin"], 10);

    // not an app command
    let err = invoke(&main, "delete_everything", serde_json::json!({})).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("not found")
            || err.to_string().contains("delete_everything"),
        "{err}"
    );
    // plugin commands that were never registered
    for cmd in [
        "plugin:shell|execute",
        "plugin:fs|read_file",
        "plugin:opener|open_path",
    ] {
        assert!(invoke(&main, cmd, serde_json::json!({})).is_err(), "{cmd}");
    }
    // core commands that exist in Tauri but are not granted
    for cmd in [
        "plugin:window|close",
        "plugin:webview|create_webview_window",
        "plugin:path|resolve_directory",
        "plugin:app|app_hide",
    ] {
        assert!(invoke(&main, cmd, serde_json::json!({})).is_err(), "{cmd}");
    }
}

#[tokio::test]
async fn ipc_rejects_app_commands_from_other_windows() {
    let f = Fixture::new(Duration::ZERO).await;
    let (app, _main) = ipc_app(&f);
    let other =
        WebviewWindowBuilder::new(&app, "other", tauri::WebviewUrl::App("index.html".into()))
            .build()
            .unwrap();
    let err = invoke(&other, "settings_get", serde_json::json!({})).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("not allowed")
            || err.to_string().to_lowercase().contains("denied")
            || err.to_string().to_lowercase().contains("permission"),
        "{err}"
    );
}

#[tokio::test]
async fn settings_set_over_ipc_refuses_program_keys() {
    let f = Fixture::new(Duration::ZERO).await;
    let (_app, main) = ipc_app(&f);
    let err = invoke(
        &main,
        "settings_set",
        serde_json::json!({"patch": {"tools.terminal": {"path": "/bin/sh", "args": []}}}),
    )
    .unwrap_err();
    assert_eq!(err["code"], "permission-denied");
    let ok = invoke(
        &main,
        "settings_set",
        serde_json::json!({"patch": {"diff.mode": "unified"}}),
    )
    .unwrap();
    assert_eq!(ok["diff.mode"], "unified");
}

#[tokio::test]
async fn opener_scope_rejects_everything_but_https() {
    let f = Fixture::new(Duration::ZERO).await;
    let (_app, main) = ipc_app(&f);
    for url in [
        "http://example.com/",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "ftp://example.com/x",
        "vscode://file/etc/passwd",
        "x-apple.systempreferences:",
        "data:text/html,hi",
    ] {
        let err = invoke(
            &main,
            "plugin:opener|open_url",
            serde_json::json!({"url": url}),
        )
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("not allowed"),
            "{url}: {err}"
        );
    }
    // other opener commands are not granted at all
    for cmd in [
        "plugin:opener|open_path",
        "plugin:opener|reveal_item_in_dir",
    ] {
        assert!(
            invoke(
                &main,
                cmd,
                serde_json::json!({"path": "/etc/passwd", "paths": ["/etc/passwd"]})
            )
            .is_err(),
            "{cmd}"
        );
    }
}
