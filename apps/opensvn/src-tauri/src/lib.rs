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
#[cfg(feature = "failpoints")]
mod crash_hook;
mod db;
mod diagnostics;
mod diff_sides;
mod display_text;
pub mod engine;
mod errors;
mod events;
mod layout;
pub mod lock_ownership;
mod logging;
mod menu;
mod notify;
mod office_diff;
mod ops;
mod os_integration;
mod path_grants;
mod preview;
mod programs;
pub mod settings;
mod status;
mod updater;
mod wake;
mod wc;

use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use app_state::{AppDeps, AppState, EngineSource};
use engine::FakeEngine;

pub use command_names::COMMAND_NAMES;
pub use command_registry::builder;

/// Simulated time per item of the fake engine's long operations (visible progress in the UI).
const FAKE_STEP_DELAY: Duration = Duration::from_millis(40);

/// Environment variable that selects the engine: `fake` runs the in-memory demo engine (frontend
/// development, end-to-end tests); `real` (or unset) runs the real engine when it is compiled in.
const ENGINE_ENV: &str = "OPENSVN_ENGINE";

fn startup_error(msg: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::other(msg.to_string()))
}

/// Which engine the app runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineKind {
    /// In-memory demo repository; its data lives in `<data>/fake-engine`.
    Fake,
    /// The Subversion crates over HTTP; uses the data directory itself.
    #[cfg(feature = "real-engine")]
    Real,
}

/// The engine to run and what to warn about once logging is up.
#[derive(Debug)]
struct EngineChoice {
    kind: EngineKind,
    warnings: Vec<String>,
}

/// Picks the engine from the value of [`ENGINE_ENV`]. The real engine is the default whenever it
/// is compiled in; the fake engine needs `OPENSVN_ENGINE=fake` then. A release build running the
/// fake engine (selected or because the real one is not compiled in) gets a loud warning: none of
/// its data reaches a real repository. An unknown value is a startup error in debug builds (a
/// typo must not silently pick the other engine during development) and a warning in release
/// builds.
fn choose_engine(env: Option<&str>, release: bool) -> Result<EngineChoice, String> {
    let real_available = cfg!(feature = "real-engine");
    let mut warnings = Vec::new();
    let requested = env.map(str::trim).filter(|v| !v.is_empty());
    let want_fake = match requested.map(str::to_ascii_lowercase).as_deref() {
        None => !real_available,
        Some("fake") => true,
        Some("real") if real_available => false,
        Some("real") => {
            warnings.push(format!(
                "{ENGINE_ENV}=real ignored: this build has no real engine (feature `real-engine`)"
            ));
            true
        }
        Some(other) if !release => {
            return Err(format!(
                "{ENGINE_ENV}={other:?} is not an engine: expected \"fake\" or \"real\""
            ));
        }
        Some(other) => {
            warnings.push(format!(
                "{ENGINE_ENV}={other:?} ignored: expected \"fake\" or \"real\""
            ));
            !real_available
        }
    };
    #[cfg(feature = "real-engine")]
    let kind = if want_fake {
        EngineKind::Fake
    } else {
        EngineKind::Real
    };
    // Without the real engine every branch above asks for the fake one.
    #[cfg(not(feature = "real-engine"))]
    let kind = {
        debug_assert!(want_fake);
        EngineKind::Fake
    };
    if kind == EngineKind::Fake && release {
        warnings.push(
            "FAKE ENGINE IN USE: this release build runs the in-memory demo engine; nothing is \
             read from or written to a real repository"
                .to_owned(),
        );
    }
    Ok(EngineChoice { kind, warnings })
}

/// Builds the engine of `kind`.
fn engine_source(kind: EngineKind) -> Result<EngineSource, Box<dyn std::error::Error>> {
    match kind {
        EngineKind::Fake => {
            let engine = tauri::async_runtime::block_on(FakeEngine::demo(FAKE_STEP_DELAY))
                .map_err(|e| startup_error(format!("fake engine: {e}")))?;
            Ok(EngineSource::Ready(Arc::new(engine)))
        }
        #[cfg(feature = "real-engine")]
        EngineKind::Real => Ok(app_state::real_engine_source()),
    }
}

/// Folder for everything the fake engine's demo data touches. The user's real `app.sqlite` is
/// never written with fake rows; the real engine uses the data directory itself.
const FAKE_ENGINE_SUBDIR: &str = "fake-engine";

/// Data directory of `kind` below the app's data directory `base`.
fn engine_data_dir(base: std::path::PathBuf, kind: EngineKind) -> std::path::PathBuf {
    match kind {
        EngineKind::Fake => base.join(FAKE_ENGINE_SUBDIR),
        #[cfg(feature = "real-engine")]
        EngineKind::Real => base,
    }
}

/// Opens storage and builds the services. Nothing here waits on I/O beyond opening the databases;
/// working copy recovery (and the fake engine's demo registration) run afterwards in the
/// background.
fn build_state<R: tauri::Runtime>(
    app: &tauri::App<R>,
    menu: Arc<menu::TauriMenu<R>>,
) -> Result<(AppState, EngineKind), Box<dyn std::error::Error>> {
    let choice = choose_engine(
        std::env::var(ENGINE_ENV).ok().as_deref(),
        !cfg!(debug_assertions),
    )
    .map_err(startup_error)?;
    let data_dir = engine_data_dir(
        db::current_data_dir(&app.config().identifier).map_err(startup_error)?,
        choice.kind,
    );
    // First thing: every later log line goes through the redacting sinks. The diagnostics bundle
    // reads the same folder.
    logging::init(&data_dir.join("logs"));
    for warning in &choice.warnings {
        tracing::warn!("{warning}");
    }
    tracing::info!(engine = ?choice.kind, "engine selected");
    let preview = preview::prepare_preview_dir(&data_dir)
        .map_err(|e| startup_error(format!("{} {}", e.message, e.detail.unwrap_or_default())))?;
    app.asset_protocol_scope()
        .allow_directory(&preview, true)
        .map_err(startup_error)?;
    let state = AppState::build(AppDeps {
        data_dir,
        engine: engine_source(choice.kind)?,
        // Repository passwords: OS credential store only (nothing is saved without one).
        credentials: auth::os_credential_store(),
        events: Arc::new(ops::TauriEvents::new(app.handle().clone())),
        picker: Arc::new(programs::NativeProgramPicker),
        status_timing: status::StatusTiming::default(),
        path_picker: Arc::new(path_grants::NativePathPicker),
        opener: Arc::new(os_integration::SystemOpener),
        menu,
        updates: Arc::new(updater::TauriUpdater::new(app.handle().clone())),
        // OS credential store only; refuses to save when none is reachable (no fallback).
        secrets: updater::os_secret_store(),
        notifier: Arc::new(notify::TauriNotifier::new(app.handle().clone())),
        launcher: Arc::new(programs::NativeLauncher),
    })
    .map_err(|e| startup_error(format!("{} {}", e.message, e.detail.unwrap_or_default())))?;
    Ok((state, choice.kind))
}

/// Runs after the window exists: with the fake engine, registers the demo working copy on first
/// start; then recovers every registered working copy (stale locks), announcing what needs the
/// user.
async fn finish_startup<R: tauri::Runtime>(handle: tauri::AppHandle<R>, kind: EngineKind) {
    let state = handle.state::<AppState>();
    if kind == EngineKind::Fake && state.wcs.list().await.map(|l| l.is_empty()).unwrap_or(true) {
        let demo_root = engine::fake::demo_wc_root();
        // Best effort: a failure here only means the sidebar starts empty.
        let _ = state.wcs.add_existing(&demo_root.to_string_lossy()).await;
    }
    state.recover_checkouts().await;
    state.recover_all().await;
    state.start_poller(None);
    state.start_wake_watch();
}

/// The generated app context (config, capabilities, assets). The macro may expand only once per
/// binary, so the app and the tests share this function.
fn app_context<R: tauri::Runtime>() -> tauri::Context<R> {
    tauri::generate_context!()
}

/// Starts the Tauri application.
pub fn run() {
    #[cfg(feature = "failpoints")]
    crash_hook::install();
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
            // A missing menu bar must not stop the app; the shortcuts work without it.
            let menu = Arc::new(menu::TauriMenu::new(app.handle().clone()));
            if let Err(e) = menu::install(app, &menu, true) {
                tracing::warn!(error = %e, "menu bar could not be installed");
            }
            let (state, kind) = build_state(app, menu)?;
            app.manage(state);
            tauri::async_runtime::spawn(finish_startup(app.handle().clone(), kind));
            Ok(())
        })
        .run(context);
    if let Err(e) = result {
        report_startup_failure(&e, show_error_dialog);
        std::process::exit(1);
    }
}

/// Last-resort report of a failed start: the log (when logging got that far), stderr, and a
/// native error dialog, since the app is usually started without a terminal.
fn report_startup_failure(error: &dyn std::fmt::Display, show: impl FnOnce(&str)) {
    let text = format!("OpenSVN failed to start: {error}");
    tracing::error!("{text}");
    eprintln!("{text}");
    show(&text);
}

/// Native error dialog for [`report_startup_failure`], shown only when a desktop session can
/// display it. Without one, rfd's gtk3 backend does not fail: `MessageDialog::show` waits
/// forever for a GTK thread that never started, and `catch_unwind` cannot help with a hang. So
/// on Linux and the BSDs the dialog is skipped when neither `DISPLAY` nor `WAYLAND_DISPLAY` is
/// set; stderr and the log already have the message.
fn show_error_dialog(text: &str) {
    if !has_display(|name| std::env::var_os(name)) {
        eprintln!("(no display; the error dialog is not shown)");
        return;
    }
    let text = text.to_owned();
    let shown = std::panic::catch_unwind(move || {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("OpenSVN")
            .set_description(text)
            .set_buttons(rfd::MessageButtons::Ok)
            .show()
    });
    if shown.is_err() {
        eprintln!("(the error dialog could not be shown)");
    }
}

/// Whether a native dialog can appear: always on Windows and macOS; on the X11/Wayland
/// platforms only with `DISPLAY` or `WAYLAND_DISPLAY` set (and not empty).
fn has_display(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    if cfg!(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )) {
        ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| var(name).is_some_and(|v| !v.is_empty()))
    } else {
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_startup_dialog_needs_a_display_on_x11_and_wayland_platforms() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert!(has_display(env(&[("DISPLAY", ":0")])));
        assert!(has_display(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        let free_desktop = cfg!(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ));
        assert_eq!(has_display(env(&[])), !free_desktop);
        assert_eq!(
            has_display(env(&[("DISPLAY", ""), ("WAYLAND_DISPLAY", "")])),
            !free_desktop
        );
    }

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

    fn choice(env: Option<&str>, release: bool) -> EngineChoice {
        choose_engine(env, release).expect("engine choice")
    }

    fn kind(env: Option<&str>, release: bool) -> EngineKind {
        choice(env, release).kind
    }

    #[test]
    fn opensvn_engine_fake_selects_the_fake_engine() {
        for env in ["fake", "FAKE", " fake "] {
            assert_eq!(kind(Some(env), false), EngineKind::Fake);
            assert!(choice(Some(env), false).warnings.is_empty());
        }
    }

    #[test]
    fn fake_engine_in_a_release_build_warns_loudly() {
        let choice = choice(Some("fake"), true);
        assert_eq!(choice.kind, EngineKind::Fake);
        assert!(
            choice
                .warnings
                .iter()
                .any(|w| w.contains("FAKE ENGINE IN USE")),
            "{:?}",
            choice.warnings
        );
    }

    #[test]
    fn unknown_engine_value_is_ignored_with_a_warning_in_release_builds() {
        let choice = choice(Some("mock"), true);
        assert_eq!(choice.kind, kind(None, true));
        assert!(
            choice.warnings[0].contains("ignored"),
            "{:?}",
            choice.warnings
        );
    }

    #[test]
    fn unknown_engine_value_is_fatal_in_debug_builds() {
        let err = choose_engine(Some("mock"), false).expect_err("typo");
        assert!(
            err.contains("OPENSVN_ENGINE") && err.contains("mock"),
            "{err}"
        );
    }

    #[test]
    fn a_startup_failure_is_shown_in_a_dialog() {
        let mut shown = None;
        report_startup_failure(&"no data directory", |t| shown = Some(t.to_owned()));
        assert_eq!(
            shown.as_deref(),
            Some("OpenSVN failed to start: no data directory")
        );
    }

    #[test]
    fn data_dir_of_the_fake_engine_is_a_subfolder() {
        let base = std::path::PathBuf::from("data");
        assert_eq!(
            engine_data_dir(base.clone(), EngineKind::Fake),
            base.join(FAKE_ENGINE_SUBDIR)
        );
    }

    #[cfg(feature = "real-engine")]
    #[test]
    fn real_engine_is_the_default_when_compiled_in() {
        for env in [None, Some(""), Some("real"), Some("Real")] {
            let choice = choice(env, true);
            assert_eq!(choice.kind, EngineKind::Real, "{env:?}");
            assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
        }
        let base = std::path::PathBuf::from("data");
        assert_eq!(engine_data_dir(base.clone(), EngineKind::Real), base);
        assert!(matches!(
            engine_source(EngineKind::Real).unwrap(),
            EngineSource::Build(_)
        ));
    }

    #[cfg(not(feature = "real-engine"))]
    #[test]
    fn without_the_real_engine_the_fake_one_runs() {
        assert_eq!(kind(None, false), EngineKind::Fake);
        assert!(choice(None, false).warnings.is_empty());
        let real = choice(Some("real"), false);
        assert_eq!(real.kind, EngineKind::Fake);
        assert!(
            real.warnings[0].contains("no real engine"),
            "{:?}",
            real.warnings
        );
        assert!(
            choice(None, true)
                .warnings
                .iter()
                .any(|w| w.contains("FAKE ENGINE IN USE"))
        );
    }

    #[test]
    fn fake_engine_source_is_ready() {
        assert!(matches!(
            engine_source(EngineKind::Fake).unwrap(),
            EngineSource::Ready(_)
        ));
    }

    #[test]
    fn committed_bindings_are_up_to_date() {
        let committed = include_str!("../../src/ipc/bindings.ts");
        assert_eq!(committed, export_to_string(), "run `pnpm gen:ipc`");
    }
}
