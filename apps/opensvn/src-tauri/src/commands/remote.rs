//! Remote operations that need no working copy (3.4).
//! `repo_cat` returns text (size-limited); `repo_save` streams to disk as an operation.

use opensvn_ipc::{
    AppError, BranchNode, CopyLocationRequest, EntryKind, Layout, LayoutDetection, LockRequest,
    LockTargets, LogGetRequest, LogPage, LogTarget, OpResult, OperationHandle, OperationKind,
    PathGrantId, RemoteEditRequest, RemoteOp, RepoEntry, RepoInfo, Rev, TextSide, UnlockRequest,
};
use tauri::State;

use crate::app_state::{AppState, leaf};
use crate::boundary::guard;
use crate::engine::{EditOp, EngineFacade, LockScope, LogScope, OpContext, RemoteEditPlan};

fn engine_err(e: svn_types::SvnError) -> AppError {
    AppError::from(&e)
}

#[tauri::command]
#[specta::specta]
pub async fn repo_info(state: State<'_, AppState>, url: String) -> Result<RepoInfo, AppError> {
    guard(async {
        let info = state
            .engine
            .repo_info(&url, &OpContext::detached())
            .await
            .map_err(engine_err)?;
        if let Err(e) = state.db.recent_url_touch(&url) {
            tracing::warn!(error = %e, "could not record recent URL");
        }
        Ok(info)
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn repo_list(
    state: State<'_, AppState>,
    url: String,
    rev: Option<Rev>,
) -> Result<Vec<RepoEntry>, AppError> {
    guard(async {
        state
            .engine
            .repo_list(&url, rev, &OpContext::detached())
            .await
            .map_err(engine_err)
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn repo_cat(
    state: State<'_, AppState>,
    url: String,
    rev: Option<Rev>,
) -> Result<TextSide, AppError> {
    guard(async {
        state
            .engine
            .repo_cat(&url, rev, &OpContext::detached())
            .await
            .map_err(engine_err)
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn repo_save(
    state: State<'_, AppState>,
    url: String,
    rev: Option<Rev>,
    grant: PathGrantId,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let save_to = state.redeem_file(&grant)?;
        let title = format!("Save {}", leaf(&url));
        Ok(
            state.start_op(OperationKind::Save, title, move |engine, ctx| async move {
                engine.repo_save(&url, rev, &save_to, &ctx).await?;
                Ok(OpResult::Done)
            }),
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn log_get(
    state: State<'_, AppState>,
    request: LogGetRequest,
) -> Result<LogPage, AppError> {
    guard(async {
        match request.target.clone() {
            LogTarget::Url { url } => state
                .engine
                .log(&LogScope::Url(url), &request, &OpContext::detached())
                .await
                .map_err(engine_err),
            LogTarget::Wc { wc_id, path } => {
                state
                    .wc_read(&wc_id, move |engine, wc| {
                        let (request, path) = (request.clone(), path.clone());
                        async move {
                            engine
                                .log(&LogScope::Wc { wc, path }, &request, &OpContext::detached())
                                .await
                        }
                    })
                    .await
            }
        }
    })
    .await
}

/// Result arrives as `op://finished` with a blame payload.
#[tauri::command]
#[specta::specta]
pub async fn blame_get(
    state: State<'_, AppState>,
    url: String,
    rev: Option<Rev>,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let title = format!("Blame {}", leaf(&url));
        Ok(
            state.start_op(OperationKind::Blame, title, move |engine, ctx| async move {
                let lines = engine.blame(&url, rev, &ctx).await?;
                Ok(OpResult::Blame { lines })
            }),
        )
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn lock(
    state: State<'_, AppState>,
    request: LockRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let (comment, steal) = (request.comment.clone(), request.steal.unwrap_or(false));
        match request.targets {
            LockTargets::Wc { wc_id, paths } => state.start_wc_op_app(
                OperationKind::Lock,
                &wc_id,
                "Lock files",
                move |engine, wc, ctx| async move {
                    let scope = LockScope::Wc { wc, paths };
                    lock_with_conflicts(engine.as_ref(), &scope, comment.as_deref(), steal, &ctx)
                        .await
                },
            ),
            LockTargets::Url { urls } => Ok(state.start_op_app(
                OperationKind::Lock,
                "Lock files",
                move |engine, ctx| async move {
                    let scope = LockScope::Urls(urls);
                    lock_with_conflicts(engine.as_ref(), &scope, comment.as_deref(), steal, &ctx)
                        .await
                },
            )),
        }
    })
    .await
}

/// Locks `scope`. A `locked-by-other` refusal carries the locks that refused it, asked from the
/// server right away (best effort: when that read fails too, the plain refusal is reported), so
/// the lock dialog can name the owner and offer an explicitly confirmed steal (doc 01 §10.4).
/// Nothing is ever stolen here: a steal only happens when the request asked for it.
async fn lock_with_conflicts(
    engine: &dyn EngineFacade,
    scope: &LockScope,
    comment: Option<&str>,
    steal: bool,
    ctx: &OpContext,
) -> Result<OpResult, AppError> {
    match engine.lock(scope, comment, steal, ctx).await {
        Ok(()) => Ok(OpResult::Done),
        Err(e) if e.code == svn_types::ErrorCode::LockedByOther => {
            let conflicts = match engine.lock_conflicts(scope, ctx).await {
                Ok(c) => c,
                Err(read) => {
                    tracing::warn!(code = ?read.code, "could not read the lock that refused a lock request");
                    Vec::new()
                }
            };
            Err(AppError::from(&e).with_lock_conflicts(conflicts))
        }
        Err(e) => Err(AppError::from(&e)),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn unlock(
    state: State<'_, AppState>,
    request: UnlockRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let break_lock = request.break_lock.unwrap_or(false);
        match request.targets {
            LockTargets::Wc { wc_id, paths } => state.start_wc_op(
                OperationKind::Unlock,
                &wc_id,
                "Unlock files",
                move |engine, wc, ctx| async move {
                    engine
                        .unlock(&LockScope::Wc { wc, paths }, break_lock, &ctx)
                        .await?;
                    Ok(OpResult::Done)
                },
            ),
            LockTargets::Url { urls } => Ok(state.start_op(
                OperationKind::Unlock,
                "Unlock files",
                move |engine, ctx| async move {
                    engine
                        .unlock(&LockScope::Urls(urls), break_lock, &ctx)
                        .await?;
                    Ok(OpResult::Done)
                },
            )),
        }
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn remote_edit(
    state: State<'_, AppState>,
    request: RemoteEditRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        if request.ops.is_empty() {
            return Err(crate::errors::invalid_argument(
                "There is nothing to change.",
            ));
        }
        let mut ops = Vec::with_capacity(request.ops.len());
        for op in request.ops {
            ops.push(match op {
                RemoteOp::Mkdir { url } => EditOp::Mkdir { url },
                RemoteOp::Rename { from_url, to_url } => EditOp::Rename { from_url, to_url },
                RemoteOp::Delete { url } => EditOp::Delete { url },
                RemoteOp::Copy {
                    from_url,
                    from_rev,
                    to_url,
                } => EditOp::Copy {
                    from_url,
                    from_rev,
                    to_url,
                },
                RemoteOp::Upload { url, source } => EditOp::Upload {
                    url,
                    local_path: state.redeem_file(&source)?,
                },
            });
        }
        let request = RemoteEditPlan {
            ops,
            message: request.message,
        };
        Ok(state.start_op(
            OperationKind::RemoteEdit,
            "Change repository",
            move |engine, ctx| async move {
                let new_rev = engine.remote_edit(&request, &ctx).await?;
                Ok(OpResult::NewRev { new_rev })
            },
        ))
    })
    .await
}

fn layout_valid(layout: &Layout) -> Result<(), AppError> {
    let Layout::Custom {
        trunk,
        branches,
        tags,
    } = layout
    else {
        return Ok(());
    };
    let bad =
        |p: &String| p.trim().is_empty() || p.len() > 500 || p.contains('\0') || p.contains("..");
    if bad(trunk) || branches.len() > 20 || tags.len() > 20 || branches.iter().chain(tags).any(bad)
    {
        return Err(crate::errors::invalid_argument(
            "The branch layout patterns are not valid.",
        ));
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn branches_list(
    state: State<'_, AppState>,
    repo_root_url: String,
) -> Result<Vec<BranchNode>, AppError> {
    guard(async {
        let ctx = OpContext::detached();
        let info = state
            .engine
            .repo_info(&repo_root_url, &ctx)
            .await
            .map_err(engine_err)?;
        let layout = state
            .db
            .layout_get(&info.repo_uuid)?
            .unwrap_or(Layout::Auto);
        state
            .engine
            .branches_list(&repo_root_url, &layout, &ctx)
            .await
            .map_err(engine_err)
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn layout_get(state: State<'_, AppState>, repo_uuid: String) -> Result<Layout, AppError> {
    guard(async { Ok(state.db.layout_get(&repo_uuid)?.unwrap_or(Layout::Auto)) }).await
}

#[tauri::command]
#[specta::specta]
pub async fn layout_set(
    state: State<'_, AppState>,
    repo_uuid: String,
    layout: Layout,
) -> Result<(), AppError> {
    guard(async {
        layout_valid(&layout)?;
        Ok(state.db.layout_set(&repo_uuid, &layout)?)
    })
    .await
}

/// Lists directories through the engine for layout detection.
struct EngineDirs(std::sync::Arc<dyn crate::engine::EngineFacade>);

#[async_trait::async_trait]
impl crate::layout::DirLister for EngineDirs {
    async fn dirs(&self, url: &str) -> Result<Vec<String>, AppError> {
        match self.0.repo_list(url, None, &OpContext::detached()).await {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter(|e| e.kind == EntryKind::Dir)
                .map(|e| e.name)
                .collect()),
            // A folder that does not exist simply has no layout below it.
            Err(e) if e.code == svn_types::ErrorCode::PathNotFound => Ok(Vec::new()),
            Err(e) => Err(AppError::from(&e)),
        }
    }
}

#[tauri::command]
#[specta::specta]
pub async fn layout_detect(
    state: State<'_, AppState>,
    repo_root_url: String,
    url: String,
) -> Result<LayoutDetection, AppError> {
    guard(async {
        let lister = EngineDirs(std::sync::Arc::clone(&state.engine));
        crate::layout::detect(&lister, &repo_root_url, &url).await
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn copy_location(
    state: State<'_, AppState>,
    request: CopyLocationRequest,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let switch_to = request.switch_wc_id.clone();
        let source = request
            .from_wc
            .as_ref()
            .map(|id| state.wcs.resolve(id).map(|r| r.wc))
            .transpose()?;
        let to_url = request.to_url.clone();
        let title = format!("Copy to {}", leaf(&request.to_url));
        match switch_to {
            None => Ok(
                state.start_op(OperationKind::Copy, title, move |engine, ctx| async move {
                    let new_rev = engine
                        .copy_location(&request, source.as_ref(), &ctx)
                        .await?;
                    Ok(OpResult::NewRev { new_rev })
                }),
            ),
            Some(wc_id) => state.start_wc_op(
                OperationKind::Copy,
                &wc_id,
                title,
                move |engine, wc, ctx| async move {
                    let new_rev = engine
                        .copy_location(&request, source.as_ref(), &ctx)
                        .await?;
                    // Switch the working copy to the location that was just created.
                    engine.switch(&wc, &to_url, Some(new_rev), &ctx).await?;
                    Ok(OpResult::NewRev { new_rev })
                },
            ),
        }
    })
    .await
}
