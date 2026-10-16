//! Commands called through the IPC layer of the mock runtime, as the WebView calls them: JSON
//! arguments in, the serialized result or `AppError` out, with the real ACL. Checks the shape the
//! TypeScript bindings expect and the error codes the UI branches on.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use opensvn_ipc::{AppError, GrantKind, NodeStatus, OperationHandle};
use serde_json::{Value, json};
use tauri::Manager;
use tauri::test::MockRuntime;

use super::fixture::{Fixture, invoke, ipc_app_with_events};
use crate::app_state::AppState;
use crate::engine::fake::demo_wc_root;
use crate::ops::events::{Recorded, RecordingEvents};
use crate::path_grants::MAIN_WINDOW;

struct Ipc {
    _app: tauri::App<MockRuntime>,
    main: tauri::WebviewWindow<MockRuntime>,
    events: Arc<RecordingEvents>,
}

impl Ipc {
    fn new(f: &Fixture) -> Self {
        let (app, main, events) = ipc_app_with_events(f);
        Self {
            _app: app,
            main,
            events,
        }
    }

    fn call(&self, cmd: &str, body: Value) -> Result<Value, Value> {
        invoke(&self.main, cmd, body)
    }

    fn state(&self) -> tauri::State<'_, AppState> {
        self.main.state::<AppState>()
    }

    /// Registers the demo working copy over IPC and returns its id.
    fn add_demo_wc(&self) -> String {
        let grant = self
            .state()
            .grants
            .mint(demo_wc_root(), GrantKind::Dir, MAIN_WINDOW)
            .id;
        let wc = self
            .call("wc_add_existing", json!({ "grant": grant.0 }))
            .unwrap();
        wc["id"].as_str().unwrap().to_owned()
    }

    /// The terminal event of `op_id` as the UI receives it: `Ok(result)` or `Err(error)`.
    async fn outcome(&self, op_id: &str) -> Result<Value, Value> {
        for _ in 0..1000 {
            for r in self.events.all() {
                match r {
                    Recorded::Finished(e) if e.op_id.0 == op_id => {
                        return Ok(serde_json::to_value(e.result).unwrap());
                    }
                    Recorded::Failed(e) if e.op_id.0 == op_id => {
                        return Err(serde_json::to_value(e.error).unwrap());
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("operation {op_id} did not end");
    }

    fn status_of(&self, wc_id: &str, path: &str) -> Option<NodeStatus> {
        let st = self
            .call("status_get", json!({ "request": { "wcId": wc_id } }))
            .unwrap();
        let st: Vec<opensvn_ipc::FileStatus> = serde_json::from_value(st).unwrap();
        st.into_iter().find(|s| s.path == path).map(|s| s.text)
    }
}

/// An operation command answers at once with an `OperationHandle` in the camelCase form of the
/// bindings.
fn handle(v: &Value, kind: &str, wc_id: Option<&str>) -> OperationHandle {
    assert!(v["opId"].is_string(), "{v}");
    assert_eq!(v["kind"], kind, "{v}");
    assert_eq!(v["background"], false, "{v}");
    assert_eq!(v["wcId"].as_str(), wc_id, "{v}");
    serde_json::from_value(v.clone()).unwrap()
}

/// The error reached the UI as an `AppError` (not a bare string) with this code.
fn code(err: &Value) -> &str {
    assert!(err["message"].is_string(), "not an AppError: {err}");
    assert!(err["retryable"].is_boolean(), "not an AppError: {err}");
    err["code"].as_str().unwrap()
}

#[tokio::test]
async fn add_paths_over_ipc() {
    let f = Fixture::new(Duration::ZERO).await;
    let ipc = Ipc::new(&f);
    let wc = ipc.add_demo_wc();
    assert_eq!(
        ipc.status_of(&wc, "notes.txt"),
        Some(NodeStatus::Unversioned)
    );

    let v = ipc
        .call("add_paths", json!({ "wcId": wc, "paths": ["notes.txt"] }))
        .unwrap();
    let h = handle(&v, "add", Some(&wc));
    assert_eq!(h.title, "Add in demo");
    assert_eq!(
        ipc.outcome(&h.op_id.0).await.unwrap(),
        json!({ "kind": "done" })
    );
    assert_eq!(ipc.status_of(&wc, "notes.txt"), Some(NodeStatus::Added));

    // Adding a versioned file is refused by the operation, reported on its failed event.
    let v = ipc
        .call("add_paths", json!({ "wcId": wc, "paths": ["README.md"] }))
        .unwrap();
    let h = handle(&v, "add", Some(&wc));
    let err = ipc.outcome(&h.op_id.0).await.unwrap_err();
    assert_eq!(code(&err), "internal");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("already under version control"),
        "{err}"
    );

    // An unknown working copy fails the call itself.
    let err = ipc
        .call("add_paths", json!({ "wcId": "nope", "paths": ["a"] }))
        .unwrap_err();
    assert_eq!(code(&err), "not-a-working-copy");
    // Arguments of the wrong type never reach the command.
    assert!(
        ipc.call("add_paths", json!({ "wcId": wc, "paths": "notes.txt" }))
            .is_err()
    );
}

#[tokio::test]
async fn diff_external_over_ipc() {
    let f = Fixture::new(Duration::ZERO).await;
    let ipc = Ipc::new(&f);
    let wc = ipc.add_demo_wc();
    let target = json!({ "target": { "kind": "wc", "wcId": wc, "path": "README.md" } });

    // No diff tool: invalid-argument with the Settings action, nothing launched.
    let err = ipc.call("diff_external", target.clone()).unwrap_err();
    assert_eq!(code(&err), "invalid-argument");
    assert_eq!(err["actions"], json!(["open-settings"]), "{err}");
    assert!(f.launcher.launched.lock().unwrap().is_empty());

    // Configured: `null` and one launch.
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
    *f.picker.pick.lock().unwrap() = Some(exe);
    f.picker.accept.store(true, Ordering::SeqCst);
    crate::commands::program_choose(
        ipc.state(),
        opensvn_ipc::ProgramKind::DiffTool,
        vec!["{left}".into(), "{right}".into()],
    )
    .await
    .unwrap();
    assert_eq!(ipc.call("diff_external", target).unwrap(), Value::Null);
    assert_eq!(f.launcher.launched.lock().unwrap().len(), 1);

    // An unknown working copy, and a target kind the bindings do not have.
    let err = ipc
        .call(
            "diff_external",
            json!({ "target": { "kind": "wc", "wcId": "nope", "path": "README.md" } }),
        )
        .unwrap_err();
    assert_eq!(code(&err), "not-a-working-copy");
    assert!(
        ipc.call(
            "diff_external",
            json!({ "target": { "kind": "shell", "cmd": "rm -rf /" } })
        )
        .is_err()
    );
    assert_eq!(f.launcher.launched.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn status_refresh_over_ipc() {
    let f = Fixture::new(Duration::ZERO).await;
    let ipc = Ipc::new(&f);
    let wc = ipc.add_demo_wc();

    for paths in [Value::Null, json!(["src/main.rs"])] {
        let v = ipc
            .call("status_refresh", json!({ "wcId": wc, "paths": paths }))
            .unwrap();
        let h = handle(&v, "status-refresh", Some(&wc));
        assert_eq!(
            ipc.outcome(&h.op_id.0).await.unwrap(),
            json!({ "kind": "done" })
        );
    }
    // The refresh published a snapshot.
    assert!(
        ipc.events
            .all()
            .iter()
            .any(|r| matches!(r, Recorded::Status(s) if s.wc_id.0 == wc))
    );

    // A path that leaves the working copy is refused before anything starts (`UnsafePath`,
    // which the UI shows as an internal error: the UI never builds such a path).
    let before = ipc.events.all().len();
    let err = ipc
        .call("status_refresh", json!({ "wcId": wc, "paths": ["../etc"] }))
        .unwrap_err();
    assert_eq!(code(&err), "internal");
    assert_eq!(err["retryable"], false, "{err}");
    assert!(
        !ipc.events.all()[before..]
            .iter()
            .any(|r| matches!(r, Recorded::Started(_))),
        "nothing started"
    );
    let err = ipc
        .call("status_refresh", json!({ "wcId": "nope" }))
        .unwrap_err();
    assert_eq!(code(&err), "not-a-working-copy");
}

#[tokio::test]
async fn menu_set_layout_over_ipc() {
    let f = Fixture::new(Duration::ZERO).await;
    let ipc = Ipc::new(&f);
    assert_eq!(
        ipc.call("menu_set_layout", json!({ "hasLayout": true }))
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        ipc.call("menu_set_layout", json!({ "hasLayout": false }))
            .unwrap(),
        Value::Null
    );
    assert_eq!(*f.menu.modes.lock().unwrap(), [true, false]);
    // Not a boolean: rejected by the IPC layer, the menu is not touched.
    assert!(
        ipc.call("menu_set_layout", json!({ "hasLayout": "yes" }))
            .is_err()
    );
    assert!(ipc.call("menu_set_layout", json!({})).is_err());
    assert_eq!(f.menu.modes.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn menu_set_view_over_ipc() {
    let f = Fixture::new(Duration::ZERO).await;
    let ipc = Ipc::new(&f);
    let view = json!({
        "showUnversioned": false,
        "showIgnored": true,
        "groupByChangelist": true,
        "tree": false,
    });
    assert_eq!(
        ipc.call("menu_set_view", json!({ "view": view })).unwrap(),
        Value::Null
    );
    assert_eq!(
        *f.menu.views.lock().unwrap(),
        [opensvn_ipc::MenuViewState {
            show_unversioned: false,
            show_ignored: true,
            group_by_changelist: true,
            tree: false,
        }]
    );
    // A missing field or a non-boolean is rejected by the IPC layer; the menu is not touched.
    for bad in [
        json!({ "view": { "showUnversioned": true } }),
        json!({ "view": { "showUnversioned": 1, "showIgnored": true, "groupByChangelist": true, "tree": false } }),
        json!({}),
    ] {
        assert!(ipc.call("menu_set_view", bad).is_err());
    }
    assert_eq!(f.menu.views.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn op_retry_now_over_ipc() {
    let f = Fixture::new(Duration::ZERO).await;
    let ipc = Ipc::new(&f);
    let wc = ipc.add_demo_wc();

    // Unknown operation: an internal error naming it.
    let err = ipc
        .call("op_retry_now", json!({ "opId": "nope" }))
        .unwrap_err();
    assert_eq!(code(&err), "internal");
    assert_eq!(err["detail"], "nope", "{err}");

    // A finished operation ignores it.
    let v = ipc.call("status_refresh", json!({ "wcId": wc })).unwrap();
    let h = handle(&v, "status-refresh", Some(&wc));
    ipc.outcome(&h.op_id.0).await.unwrap();
    assert_eq!(
        ipc.call("op_retry_now", json!({ "opId": h.op_id.0 }))
            .unwrap(),
        Value::Null
    );

    // A running operation that does not wait for the network ignores it too, and keeps nothing
    // for later (see `retry_now_is_not_kept_for_an_operation_that_does_not_wait`).
    let (go, wait_go) = tokio::sync::oneshot::channel::<()>();
    let running = ipc.state().ops.spawn(
        opensvn_ipc::OperationKind::Checkout,
        None,
        "Checkout",
        |ctx| async move {
            wait_go.await.unwrap();
            let woken = tokio::time::timeout(Duration::from_millis(150), ctx.retry_now.notified())
                .await
                .is_ok();
            if woken {
                return Err(AppError::internal("a press before the wait woke it"));
            }
            Ok(opensvn_ipc::OpResult::Done)
        },
    );
    assert_eq!(
        ipc.call("op_retry_now", json!({ "opId": running.op_id.0 }))
            .unwrap(),
        Value::Null
    );
    go.send(()).unwrap();
    assert_eq!(
        ipc.outcome(&running.op_id.0).await.unwrap(),
        json!({ "kind": "done" })
    );
    // Wrong argument shape.
    assert!(ipc.call("op_retry_now", json!({ "opId": 7 })).is_err());
}
