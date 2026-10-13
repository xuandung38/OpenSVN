//! System commands (3.5): operations, auth, accounts, settings, OS integration.

use opensvn_ipc::{
    Account, AppError, AuthResponse, OpId, OpenWith, OperationHandle, PathGrantInfo, PickRequest,
    ProgramKind, Settings, SettingsPatch, TrustedCert, TrustedCertKey, WcId,
};
use tauri::State;

use crate::app_state::AppState;
use crate::boundary::guard;
use crate::diagnostics::{BundleInputs, DiagnosticsOptions, DiagnosticsPreview};

#[tauri::command]
#[specta::specta]
pub async fn op_cancel(state: State<'_, AppState>, op_id: OpId) -> Result<(), AppError> {
    guard(async { state.ops.cancel(&op_id) }).await
}

#[tauri::command]
#[specta::specta]
pub async fn op_list(state: State<'_, AppState>) -> Result<Vec<OperationHandle>, AppError> {
    guard(async { Ok(state.ops.list()) }).await
}

#[tauri::command]
#[specta::specta]
pub async fn auth_respond(
    state: State<'_, AppState>,
    request_id: String,
    response: AuthResponse,
) -> Result<(), AppError> {
    guard(async { state.auth.respond(&request_id, response) }).await
}

#[tauri::command]
#[specta::specta]
pub async fn accounts_list(state: State<'_, AppState>) -> Result<Vec<Account>, AppError> {
    guard(async { Ok(state.db.accounts_list()?) }).await
}

#[tauri::command]
#[specta::specta]
pub async fn accounts_sign_out(state: State<'_, AppState>, realm: String) -> Result<(), AppError> {
    guard(async { state.auth.sign_out(&realm) }).await
}

#[tauri::command]
#[specta::specta]
pub async fn settings_get(state: State<'_, AppState>) -> Result<Settings, AppError> {
    guard(async { Ok(state.settings.get()) }).await
}

/// Argument of `settings_set`. Exported to TypeScript as `SettingsPatch`, but a patch that does not
/// parse (unknown or program-launching key, wrong type) is reported as an `AppError` by the
/// command instead of Tauri's raw string error.
#[derive(Debug)]
pub struct PatchArg(Result<SettingsPatch, String>);

impl specta::Type for PatchArg {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <SettingsPatch as specta::Type>::definition(types)
    }
}

impl From<SettingsPatch> for PatchArg {
    fn from(p: SettingsPatch) -> Self {
        Self(Ok(p))
    }
}

impl<'de> serde::Deserialize<'de> for PatchArg {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(d)?;
        Ok(Self(serde_json::from_value(raw).map_err(|e| e.to_string())))
    }
}

/// Applies a partial update. `SettingsPatch` has no program-launching keys (see `program_choose`);
/// a patch containing one is refused with `invalid-argument`.
#[tauri::command]
#[specta::specta]
pub async fn settings_set(
    state: State<'_, AppState>,
    patch: PatchArg,
) -> Result<Settings, AppError> {
    guard(async {
        let patch = patch.0.map_err(|why| {
            crate::errors::invalid_argument("The settings change is not valid.").with_detail(why)
        })?;
        state.settings.apply_patch(&patch)
    })
    .await
}

/// Opens a native file picker for a program setting and stores it after the user confirmed the
/// command line in a native dialog. Returns the settings unchanged when the user cancels.
#[tauri::command]
#[specta::specta]
pub async fn program_choose(
    state: State<'_, AppState>,
    kind: ProgramKind,
    args: Vec<String>,
) -> Result<Settings, AppError> {
    guard(state.programs.choose(kind, args)).await
}

/// Removes a configured program.
#[tauri::command]
#[specta::specta]
pub async fn program_clear(
    state: State<'_, AppState>,
    kind: ProgramKind,
) -> Result<Settings, AppError> {
    guard(async { state.programs.clear(kind) }).await
}

/// Shows a native file or folder dialog and returns an opaque, single-use grant for the choice
/// (`None` when cancelled). Commands that need a local path outside a working copy take the grant.
#[tauri::command]
#[specta::specta]
pub async fn path_pick(
    state: State<'_, AppState>,
    request: PickRequest,
) -> Result<Option<PathGrantInfo>, AppError> {
    guard(async {
        let Ok(_open) = state.path_dialog_open.try_lock() else {
            return Err(crate::errors::invalid_argument(
                "A file picker is already open.",
            ));
        };
        Ok(state.path_picker.pick(&request).await.map(|(path, kind)| {
            state
                .grants
                .mint(path, kind, crate::path_grants::MAIN_WINDOW)
        }))
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn certs_list(state: State<'_, AppState>) -> Result<Vec<TrustedCert>, AppError> {
    guard(async { Ok(state.db.certs_list()?) }).await
}

/// Stops trusting a certificate; the next connection asks again.
#[tauri::command]
#[specta::specta]
pub async fn certs_remove(state: State<'_, AppState>, key: TrustedCertKey) -> Result<(), AppError> {
    guard(async {
        if state.db.certs_remove(&key)? {
            Ok(())
        } else {
            Err(crate::errors::invalid_argument(
                "This certificate is not in the list.",
            ))
        }
    })
    .await
}

/// Lets the user pick the default checkout folder in a native dialog and stores it.
#[tauri::command]
#[specta::specta]
pub async fn checkout_root_choose(state: State<'_, AppState>) -> Result<Settings, AppError> {
    guard(async {
        let Ok(_open) = state.path_dialog_open.try_lock() else {
            return Err(crate::errors::invalid_argument(
                "A file picker is already open.",
            ));
        };
        let Some((path, _)) = state
            .path_picker
            .pick(&PickRequest::Folder {
                title: Some("Default checkout folder".to_owned()),
            })
            .await
        else {
            return Ok(state.settings.get());
        };
        let patch = SettingsPatch {
            general_checkout_root: Some(path.to_string_lossy().into_owned()),
            ..SettingsPatch::default()
        };
        state.settings.apply_patch(&patch)
    })
    .await
}

/// Shows an item of a working copy in the file manager.
#[tauri::command]
#[specta::specta]
pub async fn reveal_in_os(
    state: State<'_, AppState>,
    wc_id: WcId,
    path: String,
) -> Result<(), AppError> {
    guard(async {
        let wc = state.wcs.resolve(&wc_id)?;
        crate::os_integration::reveal_in_os(state.opener.as_ref(), &wc.wc.root, &path)
    })
    .await
}

/// Opens an item of a working copy with the default app, the editor or the terminal.
#[tauri::command]
#[specta::specta]
pub async fn open_external(
    state: State<'_, AppState>,
    wc_id: WcId,
    path: String,
    open_with: OpenWith,
) -> Result<(), AppError> {
    guard(async {
        let wc = state.wcs.resolve(&wc_id)?;
        let settings = state.settings.get();
        crate::os_integration::open_external(
            state.opener.as_ref(),
            &wc.wc.root,
            &path,
            open_with,
            settings.tools_editor.as_ref(),
            settings.tools_terminal.as_ref(),
        )
    })
    .await
}

/// Switches the native menu between the Branch and the Location variant.
#[tauri::command]
#[specta::specta]
pub async fn menu_set_layout(state: State<'_, AppState>, has_layout: bool) -> Result<(), AppError> {
    guard(async { state.menu.set_layout_mode(has_layout) }).await
}

/// Builds the diagnostics bundle in memory and returns every file's text for review. Nothing is
/// written yet.
#[tauri::command]
#[specta::specta]
pub async fn diagnostics_preview(
    state: State<'_, AppState>,
    options: DiagnosticsOptions,
) -> Result<DiagnosticsPreview, AppError> {
    guard(async {
        let wcs = state
            .wcs
            .list()
            .await?
            .into_iter()
            .map(|w| (w.name, w.root_path))
            .collect();
        let inputs = BundleInputs {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            settings: state.settings.get(),
            working_copies: wcs,
            home_dir: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok(),
            logs_dir: Some(state.data_dir.join("logs")),
        };
        Ok(state.diagnostics.preview(&inputs, options))
    })
    .await
}

/// Saves the previewed bundle as a zip. The destination is chosen in a native save dialog;
/// returns the saved path, or `None` when the user cancels the dialog.
#[tauri::command]
#[specta::specta]
pub async fn diagnostics_export(
    state: State<'_, AppState>,
    token: String,
) -> Result<Option<String>, AppError> {
    guard(async {
        let Ok(_open) = state.path_dialog_open.try_lock() else {
            return Err(crate::errors::invalid_argument(
                "A file picker is already open.",
            ));
        };
        let Some((dest, _)) = state
            .path_picker
            .pick(&PickRequest::SaveFile {
                suggested_name: "opensvn-diagnostics.zip".to_owned(),
                title: Some("Save diagnostics".to_owned()),
            })
            .await
        else {
            return Ok(None);
        };
        state.diagnostics.export(&token, &dest)?;
        Ok(Some(dest.to_string_lossy().into_owned()))
    })
    .await
}
