//! OpenSVN desktop app shell. Commands are stubs until the backend services land.

mod command_names;
mod command_registry;
mod commands;
mod events;

pub use command_names::COMMAND_NAMES;
pub use command_registry::builder;

/// Starts the Tauri application.
pub fn run() {
    let builder = builder();
    let result = tauri::Builder::default()
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            Ok(())
        })
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("OpenSVN failed to start: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Exports bindings the same way `pnpm gen:ipc` does.
    fn export_to_string() -> String {
        let dir = std::env::temp_dir().join(format!("opensvn-bindings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("bindings.ts");
        builder()
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
        // No extra app permissions beyond the command list plus core defaults.
        assert_eq!(perms.len(), COMMAND_NAMES.len() + 1);
    }

    #[test]
    fn committed_bindings_are_up_to_date() {
        let committed = include_str!("../../src/ipc/bindings.ts");
        assert_eq!(committed, export_to_string(), "run `pnpm gen:ipc`");
    }

    #[test]
    fn stubs_return_internal_not_implemented() {
        let err = tauri::async_runtime::block_on(commands::wc_list()).expect_err("stub");
        assert_eq!(err.code, opensvn_ipc::ErrorCode::Internal);
        assert_eq!(err.message, "not implemented");
    }
}
