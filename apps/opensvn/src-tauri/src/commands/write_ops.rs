//! Write operations on a working copy (3.3); long ones return an `OperationHandle`.
//! `add`/`delete`/`move` are named `add_paths`/`delete_paths`/`move_path` (keyword and clarity).

use opensvn_ipc::{
    AppError, CheckoutRequest, CleanupRequest, CommitRequest, Depth, ErrorCode, OpResult,
    OperationHandle, OperationKind, ResolveRequest, Rev, RevertRequest, UpdateRequest, WcFormat,
    WcId,
};
use tauri::State;

use crate::app_state::{AppState, leaf};
use crate::boundary::guard;
use crate::engine::OpContext;

fn mvp_limit(msg: &str) -> AppError {
    let mut e = AppError::internal(msg);
    e.code = ErrorCode::WcFormatUnsupported;
    e
}

#[tauri::command]
#[specta::specta]
pub async fn checkout(
    state: State<'_, AppState>,
    request: CheckoutRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        if request.format != WcFormat::V31 {
            return Err(mvp_limit("Only working copy format 31 can be created."));
        }
        if let Err(e) = state.db.recent_url_touch(&request.url) {
            tracing::warn!(error = %e, "could not record recent URL");
        }
        let title = format!("Checkout {}", leaf(&request.url));
        let wcs = std::sync::Arc::clone(&state.wcs);
        Ok(state.start_op(
            OperationKind::Checkout,
            title,
            move |engine, ctx| async move {
                let info = engine.checkout(&request, &ctx).await?;
                // Register the new working copy so the UI can open it right after `op://finished`.
                if let Err(e) = wcs.add_existing(&info.root_path).await {
                    tracing::warn!(code = ?e.code, "checked out working copy was not registered");
                }
                Ok(OpResult::NewRev {
                    new_rev: info.base_rev,
                })
            },
        ))
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn update(
    state: State<'_, AppState>,
    request: UpdateRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let wc_id = request.wc_id.clone();
        let latest = request.rev.is_none();
        let wcs = std::sync::Arc::clone(&state.wcs);
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(OperationKind::Update, &wc_id, format!("Update {name}"), {
            let wc_id = wc_id.clone();
            move |engine, wc, ctx| async move {
                let rev = engine.update(&wc, &request, &ctx).await?;
                if latest && request.paths.as_ref().is_none_or(Vec::is_empty) {
                    wcs.set_incoming(&wc_id, 0);
                }
                Ok(OpResult::NewRev { new_rev: rev })
            }
        })
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn switch(
    state: State<'_, AppState>,
    wc_id: WcId,
    url: String,
    rev: Option<Rev>,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(
            OperationKind::Switch,
            &wc_id,
            format!("Switch {name}"),
            move |engine, wc, ctx| async move {
                let new_rev = engine.switch(&wc, &url, rev, &ctx).await?;
                Ok(OpResult::NewRev { new_rev })
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn commit(
    state: State<'_, AppState>,
    request: CommitRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let wc_id = request.wc_id.clone();
        let resolved = state.wcs.resolve(&wc_id)?;
        let (db, uuid) = (state.db.clone(), resolved.row.repo_uuid.clone());
        state.start_wc_op(
            OperationKind::Commit,
            &wc_id,
            format!("Commit {}", resolved.row.name),
            move |engine, wc, ctx| async move {
                let new_rev = engine.commit(&wc, &request, &ctx).await?;
                if let Err(e) = db.commit_message_add(&uuid, &request.message) {
                    tracing::warn!(error = %e, "could not record commit message");
                }
                Ok(OpResult::NewRev { new_rev })
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn revert(
    state: State<'_, AppState>,
    request: RevertRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let wc_id = request.wc_id.clone();
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(
            OperationKind::Revert,
            &wc_id,
            format!("Revert in {name}"),
            move |engine, wc, ctx| async move {
                engine.revert(&wc, &request, &ctx).await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn add_paths(
    state: State<'_, AppState>,
    wc_id: WcId,
    paths: Vec<String>,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(
            OperationKind::Add,
            &wc_id,
            format!("Add in {name}"),
            move |engine, wc, ctx| async move {
                engine.add_paths(&wc, &paths, &ctx).await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn delete_paths(
    state: State<'_, AppState>,
    wc_id: WcId,
    paths: Vec<String>,
    keep_local: Option<bool>,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(
            OperationKind::Delete,
            &wc_id,
            format!("Delete in {name}"),
            move |engine, wc, ctx| async move {
                engine
                    .delete_paths(&wc, &paths, keep_local.unwrap_or(false), &ctx)
                    .await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn move_path(
    state: State<'_, AppState>,
    wc_id: WcId,
    from: String,
    to: String,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let title = format!("Move {}", leaf(&from));
        state.start_wc_op(
            OperationKind::Move,
            &wc_id,
            title,
            move |engine, wc, ctx| async move {
                engine.move_path(&wc, &from, &to, &ctx).await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn cleanup(
    state: State<'_, AppState>,
    request: CleanupRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let wc_id = request.wc_id.clone();
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(
            OperationKind::Cleanup,
            &wc_id,
            format!("Clean up {name}"),
            move |engine, wc, ctx| async move {
                engine.cleanup(&wc, &request, &ctx).await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn set_depth(
    state: State<'_, AppState>,
    wc_id: WcId,
    path: String,
    depth: Depth,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let title = format!("Set depth of {}", leaf(&path));
        state.start_wc_op(
            OperationKind::SetDepth,
            &wc_id,
            title,
            move |engine, wc, ctx| async move {
                engine.set_depth(&wc, &path, depth, &ctx).await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn upgrade_format(
    state: State<'_, AppState>,
    wc_id: WcId,
    to: u8,
) -> Result<OperationHandle, AppError> {
    guard(async {
        WcFormat::try_from(to).map_err(|m| mvp_limit(&m))?;
        let name = state.wcs.resolve(&wc_id)?.row.name;
        state.start_wc_op(
            OperationKind::Upgrade,
            &wc_id,
            format!("Upgrade {name}"),
            move |engine, wc, ctx| async move {
                engine.upgrade_format(&wc, to, &ctx).await?;
                Ok(OpResult::Done)
            },
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn resolve(state: State<'_, AppState>, request: ResolveRequest) -> Result<(), AppError> {
    guard(
        state.wc_write(&request.wc_id.clone(), move |engine, wc| async move {
            engine.resolve(&wc, &request, &OpContext::detached()).await
        }),
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn props_set(
    state: State<'_, AppState>,
    wc_id: WcId,
    path: String,
    name: String,
    value: Option<String>,
) -> Result<(), AppError> {
    guard(state.wc_write(&wc_id, move |engine, wc| async move {
        engine
            .props_set(&wc, &path, &name, value.as_deref(), &OpContext::detached())
            .await
    }))
    .await
}
