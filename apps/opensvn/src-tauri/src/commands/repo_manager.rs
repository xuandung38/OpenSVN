//! Repository manager (doc 01 §3.2, §6.5): the full list of registered working copies including
//! missing ones, re-locating a moved working copy, recent repository URLs and the default checkout
//! folder. Every local path comes from a native dialog as a path grant, or from Rust itself.

use opensvn_ipc::{
    AppError, GrantKind, PathGrantId, PathGrantInfo, PickRequest, Settings, WcEntry, WcId,
    WorkingCopy,
};
use tauri::State;

use crate::app_state::AppState;
use crate::boundary::guard;
use crate::errors::invalid_argument;
use crate::path_grants::MAIN_WINDOW;
use crate::wc::checkout_target::expand_root;

/// Every registered working copy, including ones whose folder is gone or moved (`missing`).
#[tauri::command]
#[specta::specta]
pub async fn wc_entries(state: State<'_, AppState>) -> Result<Vec<WcEntry>, AppError> {
    guard(state.wcs.entries()).await
}

/// Points a moved working copy at its new folder, chosen in the native folder dialog. The folder
/// must be a working copy of the same repository. Recovery runs as on open.
#[tauri::command]
#[specta::specta]
pub async fn wc_locate(
    state: State<'_, AppState>,
    wc_id: WcId,
    grant: PathGrantId,
) -> Result<WorkingCopy, AppError> {
    guard(async {
        // Fail on an unknown id before the grant is spent.
        state.wcs.resolve(&wc_id)?;
        let path = state.redeem_dir(&grant)?;
        state.wcs.locate(&wc_id, &path).await?;
        state.status.forget(&wc_id);
        state.open_wc(&wc_id).await
    })
    .await
}

/// Repository URLs used recently, newest first.
#[tauri::command]
#[specta::specta]
pub async fn recent_urls(state: State<'_, AppState>) -> Result<Vec<String>, AppError> {
    guard(async { Ok(state.db.recent_url_list(crate::db::RECENT_URL_CAP)?) }).await
}

/// Forgets a recent repository URL (the welcome screen's "Remove"); returns the list left.
#[tauri::command]
#[specta::specta]
pub async fn recent_url_remove(
    state: State<'_, AppState>,
    url: String,
) -> Result<Vec<String>, AppError> {
    guard(async {
        state.db.recent_url_remove(&url)?;
        Ok(state.db.recent_url_list(crate::db::RECENT_URL_CAP)?)
    })
    .await
}

/// A folder grant for the default checkout folder (`general.checkoutRoot`), created when missing.
/// The setting only changes through `checkout_root_choose`, so the grant always names a folder
/// the user chose (or the built-in `~/svn`).
#[tauri::command]
#[specta::specta]
pub async fn checkout_root_grant(state: State<'_, AppState>) -> Result<PathGrantInfo, AppError> {
    guard(async {
        let root = expand_root(&state.settings.get().general_checkout_root)?;
        std::fs::create_dir_all(&root).map_err(|e| {
            invalid_argument("The default checkout folder cannot be created.")
                .with_detail(e.to_string())
                .with_path(root.to_string_lossy())
        })?;
        if !root.is_dir() {
            return Err(
                invalid_argument("The default checkout folder is not a folder.")
                    .with_path(root.to_string_lossy()),
            );
        }
        Ok(state.grants.mint(root, GrantKind::Dir, MAIN_WINDOW))
    })
    .await
}

/// Lets the user choose the default checkout folder in the native folder dialog and stores it.
/// Returns the settings unchanged when the dialog is cancelled.
#[tauri::command]
#[specta::specta]
pub async fn checkout_root_choose(state: State<'_, AppState>) -> Result<Settings, AppError> {
    guard(async {
        let Ok(_open) = state.path_dialog_open.try_lock() else {
            return Err(invalid_argument("A file picker is already open."));
        };
        let request = PickRequest::Folder {
            title: Some("Choose the default checkout folder".to_owned()),
        };
        let Some((path, kind)) = state.path_picker.pick(&request).await else {
            return Ok(state.settings.get());
        };
        if kind != GrantKind::Dir || !path.is_absolute() || !path.is_dir() {
            return Err(invalid_argument("Select a folder."));
        }
        let Some(text) = path.to_str() else {
            return Err(invalid_argument(
                "The folder path contains characters that cannot be stored.",
            ));
        };
        state.settings.set_checkout_root(text)
    })
    .await
}
