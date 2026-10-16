#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opensvn_ipc::{
    AppError, GrantKind, OpResult, OperationHandle, PathGrantId, PickRequest, ProgramKind,
    WorkingCopy,
};
use serde_json::Value;
use tauri::Manager;
use tauri::WebviewWindowBuilder;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{INVOKE_KEY, MockRuntime, get_ipc_response};
use tauri::webview::InvokeRequest;

use crate::app_state::{AppDeps, AppState, EngineSource};
use crate::engine::FakeEngine;
use crate::engine::fake::demo_wc_root;
use crate::menu::MenuControl;
use crate::notify::Notifier;
use crate::ops::events::{Recorded, RecordingEvents};
use crate::os_integration::OsOpener;
use crate::path_grants::{MAIN_WINDOW, PathPicker};
use crate::programs::{ProgramLauncher, ProgramPicker};
use crate::status::StatusTiming;
use crate::updater::{MemorySecretStore, UpdateControl, UpdateInfo};
use secrecy::{ExposeSecret, SecretString};

/// Records launcher calls.
#[derive(Default)]
pub struct RecordingOpener {
    pub calls: Mutex<Vec<String>>,
}

impl OsOpener for RecordingOpener {
    fn reveal(&self, path: &std::path::Path) -> Result<(), AppError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("reveal {}", path.display()));
        Ok(())
    }
    fn open_default(&self, path: &std::path::Path) -> Result<(), AppError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("open {}", path.display()));
        Ok(())
    }
    fn run(&self, program: &str, args: &[String], cwd: &std::path::Path) -> Result<(), AppError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("run {program} {args:?} in {}", cwd.display()));
        Ok(())
    }
}

/// Records notifications.
#[derive(Default)]
pub struct RecordingNotifier {
    pub shown: Mutex<Vec<(String, String)>>,
}

impl Notifier for RecordingNotifier {
    fn notify(&self, title: &str, body: &str) {
        self.shown
            .lock()
            .unwrap()
            .push((title.to_owned(), body.to_owned()));
    }
}

/// Update double: records the token it was given.
#[derive(Default)]
pub struct RecordingUpdates {
    pub tokens: Mutex<Vec<Option<String>>>,
    pub available: Mutex<Option<UpdateInfo>>,
}

#[async_trait]
impl UpdateControl for RecordingUpdates {
    async fn check(&self, token: Option<SecretString>) -> Result<Option<UpdateInfo>, AppError> {
        self.tokens
            .lock()
            .unwrap()
            .push(token.map(|t| t.expose_secret().to_owned()));
        Ok(self.available.lock().unwrap().clone())
    }
    async fn install(&self, _token: Option<SecretString>) -> Result<(), AppError> {
        if self.available.lock().unwrap().is_some() {
            Ok(())
        } else {
            Err(crate::errors::invalid_argument("Check for updates first."))
        }
    }
}

/// Records menu layout switches.
#[derive(Default)]
pub struct RecordingMenu {
    pub modes: Mutex<Vec<bool>>,
}

impl MenuControl for RecordingMenu {
    fn set_layout_mode(&self, has_layout: bool) -> Result<(), AppError> {
        self.modes.lock().unwrap().push(has_layout);
        Ok(())
    }
}

/// File dialog double: returns the scripted choice.
#[derive(Default)]
pub struct ScriptedPaths {
    pub choice: Mutex<Option<(PathBuf, GrantKind)>>,
}

#[async_trait]
impl PathPicker for ScriptedPaths {
    async fn pick(&self, _request: &PickRequest) -> Option<(PathBuf, GrantKind)> {
        self.choice.lock().unwrap().clone()
    }
}

/// Dialog double with scripted answers.
#[derive(Default)]
pub struct Scripted {
    pub pick: Mutex<Option<PathBuf>>,
    pub accept: AtomicBool,
    pub confirmations: Mutex<Vec<String>>,
}

#[async_trait]
impl ProgramPicker for Scripted {
    async fn pick_program(&self, _kind: ProgramKind) -> Option<PathBuf> {
        self.pick.lock().unwrap().clone()
    }
    async fn confirm(&self, _kind: ProgramKind, command_line: &str) -> bool {
        self.confirmations
            .lock()
            .unwrap()
            .push(command_line.to_owned());
        self.accept.load(Ordering::SeqCst)
    }
}

/// Launcher double: records what would have been started, with the staged files' contents.
#[derive(Default)]
pub struct RecordingLauncher {
    pub launched: Mutex<Vec<(PathBuf, Vec<String>)>>,
}

impl ProgramLauncher for RecordingLauncher {
    fn launch(&self, program: &std::path::Path, args: &[String]) -> Result<(), AppError> {
        self.launched
            .lock()
            .unwrap()
            .push((program.to_path_buf(), args.to_vec()));
        Ok(())
    }
}

/// App state over `engine` in `dir` with inert doubles for everything else, as `lib.rs` builds it
/// at start-up (engine selection tests).
pub fn state_with_engine(
    dir: &std::path::Path,
    engine: EngineSource,
) -> Result<AppState, AppError> {
    AppState::build(AppDeps {
        data_dir: dir.to_path_buf(),
        engine,
        credentials: Arc::new(secure_store::MemoryStore::new()),
        events: Arc::new(RecordingEvents::default()),
        picker: Arc::new(Scripted::default()),
        status_timing: StatusTiming::default(),
        path_picker: Arc::new(ScriptedPaths::default()),
        opener: Arc::new(RecordingOpener::default()),
        notifier: Arc::new(RecordingNotifier::default()),
        menu: Arc::new(RecordingMenu::default()),
        updates: Arc::new(RecordingUpdates::default()),
        secrets: Arc::new(MemorySecretStore::default()),
        launcher: Arc::new(RecordingLauncher::default()),
    })
}

pub struct Fixture {
    pub app: tauri::App<MockRuntime>,
    pub events: Arc<RecordingEvents>,
    pub picker: Arc<Scripted>,
    pub paths: Arc<ScriptedPaths>,
    pub opener: Arc<RecordingOpener>,
    pub notifier: Arc<RecordingNotifier>,
    pub menu: Arc<RecordingMenu>,
    pub updates: Arc<RecordingUpdates>,
    pub secrets: Arc<MemorySecretStore>,
    pub launcher: Arc<RecordingLauncher>,
    pub engine: Arc<FakeEngine>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// App state over the demo fake engine with `delay` per simulated item.
    pub async fn new(delay: Duration) -> Self {
        Self::with_timing(delay, StatusTiming::default()).await
    }

    /// Like [`Fixture::new`] with custom watcher timing.
    pub async fn with_timing(delay: Duration, status_timing: StatusTiming) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(FakeEngine::demo(delay).await.unwrap());
        let events = Arc::new(RecordingEvents::default());
        let picker = Arc::new(Scripted::default());
        let paths = Arc::new(ScriptedPaths::default());
        let opener = Arc::new(RecordingOpener::default());
        let notifier = Arc::new(RecordingNotifier::default());
        let menu = Arc::new(RecordingMenu::default());
        let updates = Arc::new(RecordingUpdates::default());
        let secrets = Arc::new(MemorySecretStore::default());
        let launcher = Arc::new(RecordingLauncher::default());
        let state = AppState::build(AppDeps {
            data_dir: dir.path().to_path_buf(),
            engine: EngineSource::Ready(engine.clone()),
            credentials: Arc::new(secure_store::MemoryStore::new()),
            events: events.clone(),
            picker: picker.clone(),
            status_timing,
            path_picker: paths.clone(),
            opener: opener.clone(),
            notifier: notifier.clone(),
            menu: menu.clone(),
            updates: updates.clone(),
            secrets: secrets.clone(),
            launcher: launcher.clone(),
        })
        .unwrap();
        let app = tauri::test::mock_app();
        app.manage(state);
        Self {
            app,
            events,
            picker,
            paths,
            opener,
            notifier,
            menu,
            updates,
            secrets,
            launcher,
            engine,
            _dir: dir,
        }
    }

    pub fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    /// Registers the demo working copy.
    pub async fn add_demo_wc(&self) -> WorkingCopy {
        crate::commands::wc_add_existing(self.state(), self.grant_dir(&demo_wc_root()))
            .await
            .unwrap()
    }

    /// A folder grant for `path`, as the native dialog would mint it.
    pub fn grant_dir(&self, path: &std::path::Path) -> PathGrantId {
        self.state()
            .grants
            .mint(path.to_path_buf(), GrantKind::Dir, MAIN_WINDOW)
            .id
    }

    /// A file grant for `path`.
    pub fn grant_file(&self, path: &std::path::Path) -> PathGrantId {
        self.state()
            .grants
            .mint(path.to_path_buf(), GrantKind::File, MAIN_WINDOW)
            .id
    }

    /// Waits for the terminal event of `handle` and returns its outcome.
    pub async fn wait_op(&self, handle: &OperationHandle) -> Result<OpResult, AppError> {
        for _ in 0..1000 {
            for r in self.events.all() {
                match r {
                    Recorded::Finished(f) if f.op_id == handle.op_id => return Ok(f.result),
                    Recorded::Failed(f) if f.op_id == handle.op_id => return Err(f.error),
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("operation {} did not end", handle.op_id.0);
    }
}

/// App with the real ACL (from `capabilities/` and the generated permissions) and a `main` window,
/// over the engine and doubles of `f`.
pub fn ipc_app(f: &Fixture) -> (tauri::App<MockRuntime>, tauri::WebviewWindow<MockRuntime>) {
    let (app, main, _) = ipc_app_with_events(f);
    (app, main)
}

/// Like [`ipc_app`], with the events of its own app state.
pub fn ipc_app_with_events(
    f: &Fixture,
) -> (
    tauri::App<MockRuntime>,
    tauri::WebviewWindow<MockRuntime>,
    Arc<RecordingEvents>,
) {
    let reg = crate::builder::<MockRuntime>();
    let app = tauri::test::mock_builder()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(reg.invoke_handler())
        .build(crate::app_context())
        .unwrap();
    let events = Arc::new(RecordingEvents::default());
    let state = AppState::build(AppDeps {
        data_dir: tempfile::tempdir().unwrap().keep(),
        engine: EngineSource::Ready(f.engine.clone()),
        credentials: Arc::new(secure_store::MemoryStore::new()),
        events: events.clone(),
        picker: f.picker.clone(),
        status_timing: StatusTiming::default(),
        path_picker: f.paths.clone(),
        opener: f.opener.clone(),
        notifier: f.notifier.clone(),
        menu: f.menu.clone(),
        updates: f.updates.clone(),
        secrets: f.secrets.clone(),
        launcher: f.launcher.clone(),
    })
    .unwrap();
    app.manage(state);
    let main = WebviewWindowBuilder::new(&app, "main", tauri::WebviewUrl::App("index.html".into()))
        .build()
        .unwrap();
    (app, main, events)
}

/// Calls `cmd` through the IPC layer as the WebView does: JSON in, JSON (or the error) out.
pub fn invoke(
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
