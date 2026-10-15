//! Write operations on a working copy (3.3); long ones return an `OperationHandle`.
//! `add`/`delete`/`move` are named `add_paths`/`delete_paths`/`move_path` (keyword and clarity).

use opensvn_ipc::{
    AppError, CheckoutRequest, CleanupRequest, CommitRequest, Depth, ErrorCode, IncompleteCheckout,
    OpResult, OperationHandle, OperationKind, ResolveRequest, Rev, RevertRequest, UpdateRequest,
    WcFormat, WcId,
};
use tauri::State;

use crate::app_state::{AppState, leaf};
use crate::boundary::guard;
use crate::engine::OpContext;

/// Refusal of a command that would interfere with our own running operation `title`.
pub(crate) fn busy_with(title: &str) -> AppError {
    let mut e = AppError::internal(format!(
        "\"{title}\" is still running on this working copy. Wait for it to finish (or cancel it), \
         then try again."
    ));
    e.code = ErrorCode::WcBusy;
    e.retryable = true;
    e
}

fn mvp_limit(msg: &str) -> AppError {
    let mut e = crate::errors::invalid_argument(msg);
    e.code = ErrorCode::WcFormatUnsupported;
    e
}

/// Checks out into a new folder (`dest_name`) inside the granted parent folder, or into the granted
/// folder itself. The target must not exist or be an empty folder, unless it holds an interrupted
/// checkout of the same URL: then the checkout is refused with
/// [`AppError::incomplete_checkout`] set, so the UI offers "Continue checkout" instead.
///
/// The choices of the checkout are journaled before it starts and dropped when it finishes, so a
/// checkout interrupted in any way (connection lost for longer than the wait, cancelled, app
/// closed or killed) can be continued exactly ([`wc_continue`]). On success the working copy is
/// registered and its id returned in `OpResult::CheckedOut`. When the checkout fails or is cancelled
/// after it created the working copy, the partial working copy is registered anyway (listed as
/// incomplete, with "Continue checkout"); otherwise a folder this checkout created is removed again
/// while it is still empty. Nothing else is ever removed.
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
        // Checked before the grant is redeemed so a typo does not spend the user's folder choice.
        if let Some(name) = request.dest_name.as_deref() {
            crate::wc::checkout_target::validate_folder_name(name)?;
        }
        let dest = state.redeem_dir(&request.dest)?;
        let target = match crate::wc::checkout_target::prepare(&dest, request.dest_name.as_deref())
        {
            Ok(t) => t,
            Err(e) => {
                let wanted = request
                    .dest_name
                    .as_deref()
                    .map_or_else(|| dest.clone(), |n| dest.join(n));
                return Err(
                    match interrupted_checkout_at(&state, &wanted, &request.url).await {
                        Some(info) => e.with_incomplete_checkout(info),
                        None => e,
                    },
                );
            }
        };
        if let Err(e) = state.db.recent_url_touch(&request.url) {
            tracing::warn!(error = %e, "could not record recent URL");
        }
        let journal = crate::db::CheckoutJournal {
            root_path: canonical_root(&target.path),
            url: request.url.clone(),
            depth: depth_word(request.depth).to_owned(),
            ignore_externals: request.ignore_externals,
        };
        state.db.checkout_journal_put(&journal)?;
        let plan = crate::engine::CheckoutPlan {
            url: request.url.clone(),
            path: target.path.clone(),
            rev: request.rev,
            depth: request.depth,
            sparse: request.sparse.clone(),
            format: request.format,
            store_pristine: request.store_pristine,
            ignore_externals: request.ignore_externals,
        };
        let title = format!("Checkout {}", leaf(&request.url));
        let wcs = std::sync::Arc::clone(&state.wcs);
        let engine = std::sync::Arc::clone(&state.engine);
        let db = state.db.clone();
        let spec = crate::ops::OpSpec::new(OperationKind::Checkout, None, title).at(&target.path);
        Ok(state.ops.spawn_with(spec, move |ctx| async move {
            let info = match engine.checkout(&plan, &ctx).await {
                Ok(info) => info,
                Err(e) => {
                    // A checkout that already wrote its working copy stays resumable: it is
                    // listed as incomplete and "Continue checkout" finishes it.
                    let err = AppError::from(&e);
                    return Err(match wcs.add_unfinished(&target.path).await {
                        Some(wc) => match wc.resume {
                            Some(resume) => err.with_incomplete_checkout(IncompleteCheckout {
                                wc_id: wc.id,
                                rev: resume.rev,
                                root_path: wc.root_path,
                            }),
                            None => err,
                        },
                        None => {
                            forget_journal(&db, &journal.root_path);
                            target.discard_if_empty();
                            err
                        }
                    });
                }
            };
            forget_journal(&db, &journal.root_path);
            forget_journal(&db, &info.root_path);
            // Register the new working copy so the UI can open it right after `op://finished`.
            let wc_id = match wcs.add_existing(&info.root_path).await {
                Ok(wc) => Some(wc.id),
                Err(e) => {
                    tracing::warn!(code = ?e.code, "checked out working copy was not registered");
                    None
                }
            };
            Ok(OpResult::CheckedOut {
                wc_id,
                new_rev: info.base_rev,
            })
        }))
    })
    .await
}

/// `path` canonicalized like the engine records a working copy root; a missing last component
/// is joined to its canonical parent.
pub(crate) fn canonical_root(path: &std::path::Path) -> String {
    let resolved =
        std::fs::canonicalize(path).or_else(|_| match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent).map(|p| p.join(name)),
            _ => Err(std::io::Error::other("no parent")),
        });
    resolved
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn depth_word(depth: Depth) -> &'static str {
    match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
    }
}

fn forget_journal(db: &crate::db::AppDb, root: &str) {
    if let Err(e) = db.checkout_journal_remove(root) {
        tracing::warn!(error = %e, "could not update the checkout journal");
    }
}

fn forget_switch(db: &crate::db::AppDb, root: &str) {
    if let Err(e) = db.switch_journal_remove(root) {
        tracing::warn!(error = %e, "could not update the switch journal");
    }
}

/// The interrupted checkout of `url` at `path`, registered so it can be continued; `None` when
/// `path` holds anything else (a complete working copy, another URL, user files).
async fn interrupted_checkout_at(
    state: &AppState,
    path: &std::path::Path,
    url: &str,
) -> Option<IncompleteCheckout> {
    // Looked at first, registered only when it is the interrupted checkout of this URL.
    let info = state
        .engine
        .wc_info(path, &crate::engine::OpContext::detached())
        .await
        .ok()?;
    let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
    if !info.incomplete || info.resume_rev.is_none() || !same(&info.url, url) {
        return None;
    }
    let wc = state.wcs.add_unfinished(path).await?;
    let resume = wc.resume?;
    Some(IncompleteCheckout {
        wc_id: wc.id,
        rev: resume.rev,
        root_path: wc.root_path,
    })
}

/// Finishes an interrupted checkout, update or switch of the whole working copy at the revision
/// it was reaching (recorded in the working copy), with the depths it records and, for a
/// checkout, the externals choice the user made; a switch goes on to the URL it was switching
/// to. Fails when nothing is incomplete.
#[tauri::command]
#[specta::specta]
pub async fn wc_continue(
    state: State<'_, AppState>,
    wc_id: WcId,
) -> Result<OperationHandle, AppError> {
    guard(async {
        let row = state.wcs.resolve(&wc_id)?.row;
        let journal = state.db.checkout_journal_get(&row.root_path)?;
        let switch_to = state.db.switch_journal_get(&row.root_path)?;
        let (kind, title) = if switch_to.is_some() {
            (
                OperationKind::Switch,
                format!("Continue switch {}", row.name),
            )
        } else if journal.is_some() {
            (
                OperationKind::Update,
                format!("Continue checkout {}", row.name),
            )
        } else {
            (
                OperationKind::Update,
                format!("Continue update {}", row.name),
            )
        };
        let ignore_externals = journal.as_ref().is_some_and(|j| j.ignore_externals);
        let db = state.db.clone();
        state.start_wc_op(kind, &wc_id, title, move |engine, wc, ctx| async move {
            let info = engine.wc_info(&wc.root, &ctx).await?;
            let Some(rev) = info.resume_rev.filter(|_| info.incomplete) else {
                forget_switch(&db, &row.root_path);
                return Err(svn_types::SvnError::new(
                    svn_types::ErrorCode::WcInvalidOperation,
                    "There is nothing to continue: the working copy is complete.",
                ));
            };
            if let Some(url) = switch_to {
                // A switch that stopped half-way: an update would keep the folders it had
                // not reached at their old URL. Finished at the revision it was reaching.
                let new_rev = engine.switch(&wc, &url, Some(rev), &ctx).await?;
                forget_switch(&db, &row.root_path);
                return Ok(OpResult::NewRev { new_rev });
            }
            let new_rev = engine.resume(&wc, rev, ignore_externals, &ctx).await?;
            forget_journal(&db, &info.root_path);
            forget_journal(&db, &row.root_path);
            Ok(OpResult::NewRev { new_rev })
        })
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
        let row = state.wcs.resolve(&wc_id)?.row;
        let (name, root, db) = (row.name, row.root_path, state.db.clone());
        state.start_wc_op(OperationKind::Update, &wc_id, format!("Update {name}"), {
            let wc_id = wc_id.clone();
            move |engine, wc, ctx| async move {
                let rev = engine.update(&wc, &request, &ctx).await?;
                if request.paths.as_ref().is_none_or(Vec::is_empty) {
                    // The whole working copy is complete: an unfinished switch is no more.
                    forget_switch(&db, &root);
                    if latest {
                        wcs.set_incoming(&wc_id, 0);
                    }
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
        let row = state.wcs.resolve(&wc_id)?.row;
        let db = state.db.clone();
        state.start_wc_op(
            OperationKind::Switch,
            &wc_id,
            format!("Switch {}", row.name),
            move |engine, wc, ctx| async move {
                // Kept until the switch finishes: one that stops half-way (connection lost for
                // good, cancelled, app closed) is continued as a switch to this URL.
                if let Err(e) = db.switch_journal_put(&row.root_path, &url) {
                    tracing::warn!(error = %e, "could not record the switch");
                }
                let new_rev = engine.switch(&wc, &url, rev, &ctx).await?;
                forget_switch(&db, &row.root_path);
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
        let resolved = state.wcs.resolve(&wc_id)?;
        // The lock of an operation we are running is not stale: cleaning up now would break
        // into that operation (a checkout does not queue behind the working copy's actor).
        if let Some(op) = state.ops.running_on(&resolved.wc.root) {
            return Err(busy_with(&op.title));
        }
        let name = resolved.row.name;
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
