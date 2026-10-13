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

stub!(reveal_in_os(wc_id: WcId, path: String) -> ());
stub!(open_external(wc_id: WcId, path: String, open_with: OpenWith) -> ());
// Returns the path of the created diagnostics zip.
stub!(diagnostics_export() -> String);
