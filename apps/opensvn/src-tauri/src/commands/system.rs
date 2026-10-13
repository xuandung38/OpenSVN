//! System commands (3.5): operations, auth, accounts, settings, OS integration.

use opensvn_ipc::{
    Account, AppError, AuthResponse, OpId, OpenWith, OperationHandle, PathGrantInfo, PickRequest,
    ProgramKind, Settings, SettingsPatch, WcId,
};
use tauri::State;

use super::stub;
use crate::app_state::AppState;
use crate::boundary::guard;

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

stub!(auth_respond(request_id: String, response: AuthResponse) -> ());

#[tauri::command]
#[specta::specta]
pub async fn accounts_list(state: State<'_, AppState>) -> Result<Vec<Account>, AppError> {
    guard(async { Ok(state.db.accounts_list()?) }).await
}

stub!(accounts_sign_out(realm: String) -> ());

#[tauri::command]
#[specta::specta]
pub async fn settings_get(state: State<'_, AppState>) -> Result<Settings, AppError> {
    guard(async { Ok(state.settings.get()) }).await
}

/// Applies a partial update. Keys that launch programs are refused here; see `program_choose`.
#[tauri::command]
#[specta::specta]
pub async fn settings_set(
    state: State<'_, AppState>,
    patch: SettingsPatch,
) -> Result<Settings, AppError> {
    guard(async { state.settings.apply_patch(&patch) }).await
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

/// Shows a working copy item in the file manager. `path` is relative to the working copy root;
/// an empty path reveals the root folder itself. Items reached through a symbolic link are
/// refused, so the file manager never opens a folder outside the working copy.
#[tauri::command]
#[specta::specta]
pub async fn reveal_in_os(
    state: State<'_, AppState>,
    wc_id: WcId,
    path: String,
) -> Result<(), AppError> {
    guard(async {
        let resolved = state.wcs.resolve(&wc_id)?;
        let target = crate::os_shell::reveal_target(&resolved.wc.root, &path)?;
        state.revealer.reveal(&target)
    })
    .await
}
stub!(open_external(wc_id: WcId, path: String, open_with: OpenWith) -> ());
// Returns the path of the created diagnostics zip.
stub!(diagnostics_export() -> String);
