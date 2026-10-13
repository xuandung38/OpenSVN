//! OpenSVN desktop app backend: IPC command handlers over the app services (operations,
//! working copy actors, settings, storage) and the engine boundary.

mod app_state;
#[cfg(test)]
mod app_tests;
mod auth;
mod boundary;
mod command_names;
mod command_registry;
mod commands;
mod db;
mod diagnostics;
mod display_text;
pub mod engine;
mod errors;
mod events;
mod layout;
pub mod lock_ownership;
mod logging;
mod menu;
mod notify;
mod ops;
mod os_integration;
mod path_grants;
mod preview;
mod programs;
pub mod settings;
mod status;
mod updater;
mod wc;

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use app_state::{AppDeps, AppState};
use engine::{EngineFacade, FakeEngine};

pub use command_names::COMMAND_NAMES;
pub use command_registry::builder;

/// Simulated time per item of the fake engine's long operations (visible progress in the UI).
const FAKE_STEP_DELAY: Duration = Duration::from_millis(40);

fn startup_error(msg: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::other(msg.to_string()))
}

/// Builds the engine: the fake engine with demo data until the real engine is registered.
fn default_engine() -> Result<Arc<dyn EngineFacade>, Box<dyn std::error::Error>> {
    let engine = tauri::async_runtime::block_on(FakeEngine::demo(FAKE_STEP_DELAY))
        .map_err(|e| startup_error(format!("fake engine: {e}")))?;
    Ok(Arc::new(engine))
}

/// Folder for everything the fake engine's demo data touches. The user's real `app.sqlite` is
/// never written with fake rows; when the real engine is registered the app uses the data
/// directory itself.
const FAKE_ENGINE_SUBDIR: &str = "fake-engine";

/// Opens storage and builds the services. Nothing here waits on I/O beyond opening the databases;
/// working copy recovery and demo registration run afterwards in the background.
fn build_state<R: tauri::Runtime>(
    app: &tauri::App<R>,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let data_dir = db::current_data_dir(&app.config().identifier)
        .map_err(startup_error)?
        .join(FAKE_ENGINE_SUBDIR);
    // First thing: every later log line goes through the redacting sinks. The diagnostics bundle
    // reads the same folder.
    logging::init(&data_dir.join("logs"));
    let preview = preview::prepare_preview_dir(&data_dir)
        .map_err(|e| startup_error(format!("{} {}", e.message, e.detail.unwrap_or_default())))?;
    app.asset_protocol_scope()
        .allow_directory(&preview, true)
        .map_err(startup_error)?;
    AppState::build(AppDeps {
        data_dir,
        engine: default_engine()?,
        events: Arc::new(ops::TauriEvents::new(app.handle().clone())),
        picker: Arc::new(programs::NativeProgramPicker),
        status_timing: status::StatusTiming::default(),
        path_picker: Arc::new(path_grants::NativePathPicker),
        opener: Arc::new(os_integration::SystemOpener),
        menu: Arc::new(menu::TauriMenu::new(app.handle().clone())),
        updates: Arc::new(updater::TauriUpdater::new(app.handle().clone())),
        // OS credential store only; refuses to save when none is reachable (no fallback).
        secrets: updater::os_secret_store(),
        notifier: Arc::new(notify::TauriNotifier::new(app.handle().clone())),
    })
    .map_err(|e| startup_error(format!("{} {}", e.message, e.detail.unwrap_or_default())))
}

/// Runs after the window exists: registers the demo working copy on first start and recovers
/// every registered working copy (stale locks), announcing what needs the user.
async fn finish_startup<R: tauri::Runtime>(handle: tauri::AppHandle<R>) {
    let state = handle.state::<AppState>();
    if state.wcs.list().await.map(|l| l.is_empty()).unwrap_or(true) {
        let demo_root = engine::fake::demo_wc_root();
        // Best effort: a failure here only means the sidebar starts empty.
        let _ = state.wcs.add_existing(&demo_root.to_string_lossy()).await;
    }
    state.recover_all().await;
    state.start_poller(None);
}

/// The generated app context (config, capabilities, assets). The macro may expand only once per
/// binary, so the app and the tests share this function.
fn app_context<R: tauri::Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

/// Starts the Tauri application.
pub fn run() {
    let builder = builder::<tauri::Wry>();
    let context = app_context::<tauri::Wry>();
    // The updater plugin fails to start without its `plugins.updater` config (public key and
    // endpoints), so it is registered only when the config has one.
    let has_updater = context.config().plugins.0.contains_key("updater");
    let app_builder = tauri::Builder::default();
    let app_builder = if has_updater {
        app_builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        app_builder
    };
    let result = app_builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .on_window_event(|window, event| {
            // Native drag and drop: the WebView only ever sees grants, never the dropped paths.
            if window.label() != path_grants::MAIN_WINDOW {
                return;
            }
            if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, position }) =
                event
            {
                window
                    .state::<AppState>()
                    .handle_drop(paths, position.x, position.y);
            }
        })
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            menu::install(app, true)?;
            let state = build_state(app)?;
            app.manage(state);
            tauri::async_runtime::spawn(finish_startup(app.handle().clone()));
            Ok(())
        })
        .run(context);
    if let Err(e) = result {
        eprintln!("OpenSVN failed to start: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Exports bindings the same way `pnpm gen:ipc` does, into a directory unique to this call.
    fn export_to_string() -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("opensvn-bindings-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("bindings.ts");
        builder::<tauri::Wry>()
            .export(specta_typescript::Typescript::default(), &file)
            .expect("export bindings");
        let out = std::fs::read_to_string(&file).expect("read bindings");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn every_command_name_is_exported_and_permitted() {
        let bindings = export_to_string();
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).expect("json");
        let perms: Vec<&str> = capability["permissions"]
            .as_array()
            .expect("permissions")
            .iter()
            .filter_map(|p| p.as_str())
            .collect();
        for name in COMMAND_NAMES {
            assert!(
                bindings.contains(&format!("__TAURI_INVOKE(\"{name}\"")),
                "{name} missing in bindings"
            );
            let perm = format!("allow-{}", name.replace('_', "-"));
            assert!(
                perms.contains(&perm.as_str()),
                "{perm} missing in capability"
            );
        }
        // Besides the app commands, only the event subscription permissions are granted.
        let core: Vec<&str> = perms
            .iter()
            .copied()
            .filter(|p| p.starts_with("core:"))
            .collect();
        assert_eq!(
            core,
            ["core:event:allow-listen", "core:event:allow-unlisten"]
        );
        assert_eq!(perms.len(), COMMAND_NAMES.len() + core.len());
    }

    #[test]
    fn committed_bindings_are_up_to_date() {
        let committed = include_str!("../../src/ipc/bindings.ts");
        assert_eq!(committed, export_to_string(), "run `pnpm gen:ipc`");
    }
}
