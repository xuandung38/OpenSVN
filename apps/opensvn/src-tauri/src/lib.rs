//! OpenSVN desktop app backend: IPC command handlers over the app services (operations,
//! working copy actors, settings, storage) and the engine boundary.

mod app_state;
#[cfg(test)]
mod app_tests;
mod boundary;
mod command_names;
mod command_registry;
mod commands;
mod db;
pub mod engine;
mod events;
pub mod lock_ownership;
mod ops;
mod programs;
pub mod settings;
mod status;
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

/// Opens storage, builds the services and registers the demo working copy on first start.
fn build_state<R: tauri::Runtime>(
    app: &tauri::App<R>,
) -> Result<AppState, Box<dyn std::error::Error>> {
    let data_dir = db::current_data_dir().map_err(startup_error)?;
    let state = AppState::build(AppDeps {
        data_dir,
        engine: default_engine()?,
        events: Arc::new(ops::TauriEvents::new(app.handle().clone())),
        picker: Arc::new(programs::NativeProgramPicker),
    })
    .map_err(|e| startup_error(format!("{} {}", e.message, e.detail.unwrap_or_default())))?;
    let demo_root = engine::fake::demo_wc_root();
    tauri::async_runtime::block_on(async {
        state.recover_all().await;
        if state.wcs.list().await.map(|l| l.is_empty()).unwrap_or(true) {
            // Best effort: a failure here only means the sidebar starts empty.
            let _ = state.wcs.add_existing(&demo_root.to_string_lossy()).await;
        }
    });
    Ok(state)
}

/// The generated app context (config, capabilities, assets). The macro may expand only once per
/// binary, so the app and the tests share this function.
fn app_context<R: tauri::Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

/// Starts the Tauri application.
pub fn run() {
    let builder = builder::<tauri::Wry>();
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            let state = build_state(app)?;
            app.manage(state);
            Ok(())
        })
        .run(app_context::<tauri::Wry>());
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

    #[test]
    fn commands_of_later_services_are_still_stubs() {
        let err = tauri::async_runtime::block_on(commands::diagnostics_export()).expect_err("stub");
        assert_eq!(err.code, opensvn_ipc::ErrorCode::Internal);
        assert_eq!(err.message, "not implemented");
    }
}
