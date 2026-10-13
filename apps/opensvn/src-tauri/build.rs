//! Generates the permission manifest so that only the app's own commands are invocable,
//! and only through the capabilities listed in `capabilities/`.

include!("src/command_names.rs");

fn main() {
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMAND_NAMES));
    if let Err(e) = tauri_build::try_build(attributes) {
        eprintln!("tauri-build failed: {e:#}");
        std::process::exit(1);
    }
}
