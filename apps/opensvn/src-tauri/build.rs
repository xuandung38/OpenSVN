//! Generates the permission manifest so that only the app's own commands are invocable,
//! and only through the capabilities listed in `capabilities/`.

include!("src/command_names.rs");

/// Fails the build when any Tauri config enables an updater `dangerous*` option: the updater
/// sends the release token, so its TLS must never be relaxed (default-deny).
fn refuse_dangerous_updater_flags() {
    let Ok(dir) = std::fs::read_dir(".") else {
        return;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("tauri") && name.ends_with(".conf.json")) {
            continue;
        }
        println!("cargo:rerun-if-changed={name}");
        let text = match std::fs::read_to_string(entry.path()) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("cannot read {name}: {e}");
                std::process::exit(1);
            }
        };
        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("cannot parse {name}: {e}");
                std::process::exit(1);
            }
        };
        let updater = json.pointer("/plugins/updater").and_then(|u| u.as_object());
        for (key, value) in updater.into_iter().flatten() {
            if key.starts_with("dangerous") && value != &serde_json::Value::Bool(false) {
                eprintln!("{name}: plugins.updater.{key} must not be set");
                std::process::exit(1);
            }
        }
    }
}

fn main() {
    refuse_dangerous_updater_flags();
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMAND_NAMES));
    if let Err(e) = tauri_build::try_build(attributes) {
        eprintln!("tauri-build failed: {e:#}");
        std::process::exit(1);
    }
}
