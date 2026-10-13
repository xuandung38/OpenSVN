//! Working copy registry (3.1) and status / local changes (3.2).

use opensvn_ipc::{
    AppError, DiffGetRequest, DiffTarget, FileDiff, FileStatus, IgnoreAddRequest, NodeStatus,
    OpResult, OperationHandle, OperationKind, StatusGetRequest, WcId, WorkingCopy,
};
use tauri::State;

use crate::app_state::AppState;
use crate::boundary::guard;
use crate::engine::{DiffOptions, StatusQuery};

#[tauri::command]
#[specta::specta]
pub async fn wc_list(state: State<'_, AppState>) -> Result<Vec<WorkingCopy>, AppError> {
    guard(state.wcs.list()).await
}

#[tauri::command]
#[specta::specta]
pub async fn wc_add_existing(
    state: State<'_, AppState>,
    path: String,
) -> Result<WorkingCopy, AppError> {
    guard(async {
        let wc = state.wcs.add_existing(&path).await?;
        // Run recovery now so a stale lock is handled before the first operation.
        state.open_wc(&wc.id).await
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn wc_remove(state: State<'_, AppState>, wc_id: WcId) -> Result<(), AppError> {
    guard(async {
        state.wcs.remove(&wc_id)?;
        state.status.forget(&wc_id);
        Ok(())
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn wc_rename(
    state: State<'_, AppState>,
    wc_id: WcId,
    name: String,
) -> Result<(), AppError> {
    guard(async { state.wcs.rename(&wc_id, &name) }).await
}

/// Opens a working copy and refreshes its status in the background; the Changes tab shows the
/// cached snapshot immediately and receives the delta as `wc://status-changed`.
#[tauri::command]
#[specta::specta]
pub async fn wc_open(state: State<'_, AppState>, wc_id: WcId) -> Result<WorkingCopy, AppError> {
    guard(async {
        let wc = state.open_wc(&wc_id).await?;
        let resolved = state.wcs.resolve(&wc_id)?;
        let publisher = std::sync::Arc::clone(&state.status);
        tokio::spawn(async move {
            if let Err(e) = publisher.publish(&resolved).await {
                tracing::warn!(code = ?e.code, "background status scan failed");
            }
        });
        Ok(wc)
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn wc_close(state: State<'_, AppState>, wc_id: WcId) -> Result<(), AppError> {
    guard(async { state.wcs.close(&wc_id) }).await
}

/// Status from the engine, or from the snapshot while a write is running on the working copy
/// (the running operation in `op_list` is the "updating" indicator).
#[tauri::command]
#[specta::specta]
pub async fn status_get(
    state: State<'_, AppState>,
    request: StatusGetRequest,
) -> Result<Vec<FileStatus>, AppError> {
    guard(async {
        let query = StatusQuery {
            include_ignored: request.include_ignored.unwrap_or(false),
            include_unversioned: request.include_unversioned.unwrap_or(true),
        };
        let resolved = state.wcs.resolve(&request.wc_id)?;
        if resolved.actor.is_writing()
            && let Some(cached) = state.status.cached(&request.wc_id)?
        {
            return Ok(cached
                .into_iter()
                .filter(|s| match s.text {
                    NodeStatus::Ignored => query.include_ignored,
                    NodeStatus::Unversioned => query.include_unversioned,
                    _ => true,
                })
                .collect());
        }
        state
            .wc_read(&request.wc_id, move |engine, wc| async move {
                engine
                    .status(&wc, query, &crate::engine::OpContext::detached())
                    .await
            })
            .await
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn status_refresh(
    state: State<'_, AppState>,
    wc_id: WcId,
    paths: Option<Vec<String>>,
) -> Result<OperationHandle, AppError> {
    guard(async {
        // Every path must be a valid working copy path even though the scan covers the whole tree.
        for p in paths.iter().flatten() {
            svn_types::RelPath::new(p).map_err(|e| AppError::from(&e))?;
        }
        let resolved = state.wcs.resolve(&wc_id)?;
        let publisher = std::sync::Arc::clone(&state.status);
        let title = format!("Refresh {}", resolved.row.name);
        Ok(state.ops.spawn(
            OperationKind::StatusRefresh,
            Some(wc_id),
            title,
            move |_ctx| async move {
                publisher.publish(&resolved).await?;
                Ok(OpResult::Done)
            },
        ))
    })
    .await
}

/// Checks the repository for newer revisions; announces `wc://incoming` and updates remote info.
#[tauri::command]
#[specta::specta]
pub async fn status_remote(
    state: State<'_, AppState>,
    wc_id: WcId,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let resolved = state.wcs.resolve(&wc_id)?;
        let (engine, publisher, wcs) = (
            std::sync::Arc::clone(&state.engine),
            std::sync::Arc::clone(&state.status),
            std::sync::Arc::clone(&state.wcs),
        );
        let title = format!("Check {} for updates", resolved.row.name);
        Ok(state.ops.spawn(
            OperationKind::StatusRemote,
            Some(wc_id.clone()),
            title,
            move |ctx| async move {
                let wc = resolved.wc.clone();
                let scan = resolved
                    .actor
                    .read(|| {
                        let (engine, wc, ctx) =
                            (std::sync::Arc::clone(&engine), wc.clone(), ctx.clone());
                        async move {
                            engine
                                .status_remote(&wc, &ctx)
                                .await
                                .map_err(|e| AppError::from(&e))
                        }
                    })
                    .await?;
                wcs.set_incoming(&wc_id, scan.incoming);
                publisher.apply_remote(&wc_id, &scan)?;
                Ok(OpResult::Done)
            },
        ))
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn diff_get(
    state: State<'_, AppState>,
    request: DiffGetRequest,
) -> Result<FileDiff, AppError> {
    guard(async {
        let opts = DiffOptions {
            ignore_whitespace: request.ignore_whitespace.unwrap_or(false),
            force_large: request.force_large.unwrap_or(false),
        };
        match request.target {
            DiffTarget::Wc { wc_id, path } => {
                state
                    .wc_read(&wc_id, move |engine, wc| {
                        let path = path.clone();
                        async move {
                            engine
                                .diff_wc(
                                    &wc,
                                    &path,
                                    None,
                                    opts,
                                    &crate::engine::OpContext::detached(),
                                )
                                .await
                        }
                    })
                    .await
            }
            DiffTarget::WcVsRev { wc_id, path, rev } => {
                state
                    .wc_read(&wc_id, move |engine, wc| {
                        let path = path.clone();
                        async move {
                            engine
                                .diff_wc(
                                    &wc,
                                    &path,
                                    Some(rev),
                                    opts,
                                    &crate::engine::OpContext::detached(),
                                )
                                .await
                        }
                    })
                    .await
            }
            DiffTarget::Rev { url, rev } => state
                .engine
                .diff_rev(&url, None, rev, opts, &crate::engine::OpContext::detached())
                .await
                .map_err(|e| AppError::from(&e)),
            DiffTarget::Range { url, from, to } => state
                .engine
                .diff_rev(
                    &url,
                    Some(from),
                    to,
                    opts,
                    &crate::engine::OpContext::detached(),
                )
                .await
                .map_err(|e| AppError::from(&e)),
        }
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn changelist_set(
    state: State<'_, AppState>,
    wc_id: WcId,
    paths: Vec<String>,
    name: Option<String>,
) -> Result<(), AppError> {
    guard(state.wc_write(&wc_id, move |engine, wc| async move {
        engine
            .changelist_set(
                &wc,
                &paths,
                name.as_deref(),
                &crate::engine::OpContext::detached(),
            )
            .await
    }))
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn ignore_add(
    state: State<'_, AppState>,
    request: IgnoreAddRequest,
) -> Result<(), AppError> {
    guard(
        state.wc_write(&request.wc_id.clone(), move |engine, wc| async move {
            engine
                .ignore_add(&wc, &request, &crate::engine::OpContext::detached())
                .await
        }),
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn mark_move(
    state: State<'_, AppState>,
    wc_id: WcId,
    from: String,
    to: String,
) -> Result<(), AppError> {
    guard(state.wc_write(&wc_id, move |engine, wc| async move {
        engine
            .mark_move(&wc, &from, &to, &crate::engine::OpContext::detached())
            .await
    }))
    .await
}
