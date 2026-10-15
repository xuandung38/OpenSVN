//! Working copy registry (3.1) and status / local changes (3.2).

use opensvn_ipc::{
    AppError, DiffGetRequest, DiffSideKind, DiffTarget, FileDiff, FileStatus, IgnoreAddRequest,
    NodeStatus, OpResult, OperationHandle, OperationKind, PathGrantId, StatusGetRequest, WcId,
    WorkingCopy,
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
    grant: PathGrantId,
) -> Result<WorkingCopy, AppError> {
    guard(async {
        let path = state.redeem_dir(&grant)?;
        let wc = state.wcs.add_existing(&path.to_string_lossy()).await?;
        // Run recovery now so a stale lock is handled before the first operation.
        state.open_wc(&wc.id).await
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn wc_remove(state: State<'_, AppState>, wc_id: WcId) -> Result<(), AppError> {
    guard(async {
        state.watchers.stop(&wc_id.0);
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
        state.wcs.mark_open(&wc_id);
        let resolved = state.wcs.resolve(&wc_id)?;
        let publisher = std::sync::Arc::clone(&state.status);
        let scan = resolved.clone();
        tokio::spawn(async move {
            if let Err(e) = publisher.publish(&scan).await {
                tracing::warn!(code = ?e.code, "background status scan failed");
            }
        });
        state.watchers.start(resolved);
        Ok(wc)
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn wc_close(state: State<'_, AppState>, wc_id: WcId) -> Result<(), AppError> {
    guard(async {
        state.watchers.stop(&wc_id.0);
        state.wcs.close(&wc_id)
    })
    .await
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
                crate::status::run_remote_check(&engine, &publisher, &wcs, &resolved, &ctx).await?;
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
            full_text: false,
        };
        // Word and Excel files are compared as their extracted text.
        if let Some(diff) = crate::office_diff::office_diff(&state, &request.target, opts).await? {
            return Ok(diff);
        }
        fetch_diff(&state, request.target, opts).await
    })
    .await
}

/// Opens `target` in the configured external diff tool: both sides are written as read-only
/// copies into a private folder and the tool is started on them. Used when a diff is too complex
/// or too large to show, or on request. A tool set for the file's extension
/// (`diff.toolsByExtension`) gets the files as they are, binary ones included; `diff.externalTool`
/// gets text only and binary files are refused.
#[tauri::command]
#[specta::specta]
pub async fn diff_external(state: State<'_, AppState>, target: DiffTarget) -> Result<(), AppError> {
    guard(open_external_diff(&state, target)).await
}

/// Opens one version of `target` (the older or the newer side) with the default application, as
/// a read-only copy. Only document types the default application may open are accepted.
#[tauri::command]
#[specta::specta]
pub async fn diff_open_version(
    state: State<'_, AppState>,
    target: DiffTarget,
    side: DiffSideKind,
) -> Result<(), AppError> {
    guard(crate::diff_sides::open_version(&state, target, side)).await
}

pub(crate) async fn open_external_diff(
    state: &AppState,
    target: DiffTarget,
) -> Result<(), AppError> {
    use crate::programs::external_diff::{STALE_AFTER, expand_args, stage_sides, sweep_stale};
    // Fails before any engine work when no tool is configured.
    let (program, args, by_extension) = state
        .programs
        .external_diff_tool_for(&crate::diff_sides::target_path(&target))?;
    if by_extension {
        // A tool chosen for this file type gets both files as they are, binary or not.
        let (l, r) = crate::diff_sides::stage_raw_sides(state, &target).await?;
        let args = expand_args(&args, &l.to_string_lossy(), &r.to_string_lossy());
        return state.launcher.launch(&program, &args);
    }
    let opts = DiffOptions {
        ignore_whitespace: false,
        force_large: true,
        full_text: true,
    };
    let diff = fetch_diff(state, target, opts).await?;
    if diff.binary {
        return Err(crate::errors::invalid_argument(
            "Binary files cannot be opened in the external diff tool.",
        )
        .with_path(diff.path));
    }
    // Non-binary means valid UTF-8 (`is_binary` treats anything else as binary), and text sides
    // are built from those bytes unchanged, so the staged copies are the file's exact bytes.
    // TODO(p15): files in other text encodings are reported as binary and refused here; stage
    // the raw bytes once the engines expose them for external diffs.
    let side = |s: &Option<opensvn_ipc::TextSide>| {
        s.as_ref()
            .map_or(Vec::new(), |t| t.content.clone().into_bytes())
    };
    let (left, right) = (side(&diff.left), side(&diff.right));
    let root =
        crate::preview::prepare_private_dir(&state.data_dir, crate::preview::EXTERNAL_DIFF_DIR)?;
    sweep_stale(&root, STALE_AFTER);
    let (l, r) = stage_sides(&root, &diff.path, &left, &right)?;
    let args = expand_args(&args, &l.to_string_lossy(), &r.to_string_lossy());
    state.launcher.launch(&program, &args)
}

/// Runs a diff for any target: working copy targets go through the working copy's actor.
pub(crate) async fn fetch_diff(
    state: &AppState,
    target: DiffTarget,
    opts: DiffOptions,
) -> Result<FileDiff, AppError> {
    let detached = crate::engine::OpContext::detached;
    match target {
        DiffTarget::Wc { wc_id, path } => {
            state
                .wc_read(&wc_id, move |engine, wc| {
                    let path = path.clone();
                    async move { engine.diff_wc(&wc, &path, None, opts, &detached()).await }
                })
                .await
        }
        DiffTarget::WcVsRev { wc_id, path, rev } => {
            state
                .wc_read(&wc_id, move |engine, wc| {
                    let path = path.clone();
                    async move {
                        engine
                            .diff_wc(&wc, &path, Some(rev), opts, &detached())
                            .await
                    }
                })
                .await
        }
        DiffTarget::Rev { url, rev } => state
            .engine
            .diff_rev(&url, None, rev, opts, &detached())
            .await
            .map_err(|e| AppError::from(&e)),
        DiffTarget::Range { url, from, to } => state
            .engine
            .diff_rev(&url, Some(from), to, opts, &detached())
            .await
            .map_err(|e| AppError::from(&e)),
    }
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
