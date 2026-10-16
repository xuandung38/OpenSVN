//! Remote operations that need no working copy (3.4).
//! `repo_cat` returns text (size-limited); `repo_save` streams to disk as an operation.

use opensvn_ipc::{
    AppError, BranchNode, CopyLocationRequest, Layout, LayoutDetection, LockRequest, LockTargets,
    LogGetRequest, LogPage, LogTarget, OpResult, OperationHandle, OperationKind, PathGrantId,
    RemoteEditRequest, RemoteOp, RepoInfo, RepoListing, ResolvedRepoUrl, Rev, TextSide,
    UnlockRequest,
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

/// Turns a VisualSVN web-interface link (`https://host/!/#Repo/view/head/trunk`) into the
/// repository URL it shows (`https://host/svn/Repo/trunk`), confirmed by asking the server.
/// Any other input comes back unchanged so the usual URL checks report it. For an `r<N>` link
/// the server is asked about the path at revision N (peg revision), not at HEAD.
///
/// The probe goes to the scheme, host and port the user typed (the parser never changes them)
/// through the normal connect path: the user's sign-in prompts and certificate trust apply.
/// A failure is `invalid-argument` with detail [`VISUALSVN_WEB_LINK`], whatever the cause, so
/// the UI can tell the user to copy the repository URL from VisualSVN instead.
#[tauri::command]
#[specta::specta]
pub async fn repo_resolve_url(
    state: State<'_, AppState>,
    url: String,
) -> Result<ResolvedRepoUrl, AppError> {
    guard(async {
        let Some(parsed) = svn_types::parse_visualsvn_web_link(&url) else {
            return Ok(ResolvedRepoUrl {
                url,
                converted: false,
                peg_rev: None,
            });
        };
        let link = parsed.map_err(web_link_err)?;
        let candidate = link.repo_url.to_string();
        let peg_rev = link.peg_rev.map(|r| Rev(r.0));
        // An `r<N>` link shows the path as it was at N: probe it there, since it may have been
        // deleted or replaced since.
        state
            .engine
            .repo_info_at(&candidate, peg_rev, &OpContext::detached())
            .await
            .map_err(|e| match e.code {
                svn_types::ErrorCode::Cancelled => engine_err(e),
                _ => web_link_err(e),
            })?;
        Ok(ResolvedRepoUrl {
            url: candidate,
            converted: true,
            peg_rev,
        })
    })
    .await
}

/// `detail` of the error `repo_resolve_url` returns when a web link cannot be converted.
pub const VISUALSVN_WEB_LINK: &str = "visualsvn-web-link";

fn web_link_err(e: svn_types::SvnError) -> AppError {
    let mut err = crate::errors::invalid_argument(format!(
        "This is a VisualSVN web-interface link, not a repository URL, and no repository was \
         found for it ({}). In VisualSVN, use \"Copy URL to clipboard\" and paste that URL.",
        e.message
    ));
    err.detail = Some(VISUALSVN_WEB_LINK.to_owned());
    err
}

#[tauri::command]
#[specta::specta]
pub async fn repo_list(
    state: State<'_, AppState>,
    url: String,
    rev: Option<Rev>,
) -> Result<RepoListing, AppError> {
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
        let scope = match request.target.clone() {
            LogTarget::Url { url } => LogScope::Url(url),
            LogTarget::Wc { wc_id, path } => {
                // Only the location lookup reads the working copy. The log itself can take
                // minutes (a revision with 100k changed paths behind authz), and holding the
                // read path that long would queue every write and, behind it, every later read.
                let loc = state
                    .wc_read(&wc_id, move |engine, wc| {
                        let path = path.clone();
                        async move {
                            engine
                                .node_location(&wc, &path, &OpContext::detached())
                                .await
                        }
                    })
                    .await?;
                LogScope::Repo(loc)
            }
        };
        // Identical requests (the History bar, the History screen and the Incoming list all
        // reload after an update) share one read of the server, and finished pages are kept
        // until a revision moves.
        let key = crate::log_cache::LogKey::new(&scope, &request);
        let engine = std::sync::Arc::clone(&state.engine);
        state
            .status
            .logs()
            .get(key, || async move {
                engine
                    .log(&scope, &request, &OpContext::detached())
                    .await
                    .map_err(engine_err)
            })
            .await
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
    at_url: Option<String>,
) -> Result<Vec<BranchNode>, AppError> {
    guard(async {
        // Read from popovers and dialogs in the background: never raise a credential or
        // certificate prompt for it.
        let ctx = OpContext::background();
        let info = state
            .engine
            .repo_info(&repo_root_url, &ctx)
            .await
            .map_err(engine_err)?;
        // A stored layout wins. Otherwise list around `at_url`: the root-level convention, or only
        // the project folder it is in (never every top-level folder of the repository).
        let layout = match (state.db.layout_get(&info.repo_uuid)?, at_url) {
            (Some(stored), _) => stored,
            (None, Some(url)) => crate::layout::layout_near(&repo_root_url, &url),
            (None, None) => Layout::Auto,
        };
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

#[tauri::command]
#[specta::specta]
pub async fn layout_detect(
    state: State<'_, AppState>,
    repo_root_url: String,
    url: String,
) -> Result<LayoutDetection, AppError> {
    guard(async {
        let lister = crate::layout::EngineDirs(state.engine.as_ref());
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
                    // Switch the working copy to the location that was just created. A copy of
                    // the working copy's own location shares its history; anything else is
                    // refused like any switch, after the copy.
                    engine
                        .switch(&wc, &to_url, Some(new_rev), false, &ctx)
                        .await?;
                    Ok(OpResult::NewRev { new_rev })
                },
            ),
        }
    })
    .await
}
