//! `update` and `switch` on existing working copies.

use std::path::Path;
use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RepoUrl, Result, Revnum, SvnError};

use crate::ctx::{ClientCtx, UpdateSummary};
use crate::update::{UpdateKind, UpdateParams, drive_update};
use crate::wc;

async fn open(ctx: &ClientCtx, path: &Path) -> Result<(Arc<svn_wc::db::WcDb>, svn_types::RelPath)> {
    let (owner, path) = (ctx.lock_owner.clone(), path.to_path_buf());
    let (db, rel) = tokio::task::spawn_blocking(move || wc::open_containing(&owner, &path))
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??;
    Ok((Arc::new(db), rel))
}

/// A file external is updated through its definition, never on its own: a drive on it would
/// turn it into an ordinary (switched) file.
fn refuse_file_external(db: &svn_wc::db::WcDb, rel: &svn_types::RelPath) -> Result<()> {
    if db.read_base(rel)?.is_some_and(|n| n.file_external) {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{rel}' is a file external; update the folder that defines it instead"),
        ));
    }
    Ok(())
}

/// Processes the externals below `rel` after a drive (see [`crate::externals`]).
async fn with_externals(
    ctx: &ClientCtx,
    db: &Arc<svn_wc::db::WcDb>,
    rel: &svn_types::RelPath,
    depth: Depth,
    mut summary: UpdateSummary,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    let ext = crate::externals::process(ctx, db, rel, depth, cancel).await?;
    summary.events.extend(ext.events);
    summary.externals = ext.reports;
    Ok(summary)
}

/// Updates the working copy item at `path` to `revision` (HEAD when `None`).
///
/// `depth` works like `svn update --depth`: `Unknown` updates to the depths recorded in the
/// working copy; a narrower depth limits the update (nodes outside it keep their revision, and
/// children the server added there are recorded as `not-present`); it is never recorded and
/// never widens the working copy. Changing the recorded depth (`--set-depth`) is not supported.
/// Externals defined at or below `path` are processed afterwards when [`ClientCtx::externals`]
/// is set.
pub async fn update(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    revision: Option<Revnum>,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    let (db, rel) = open(ctx, path).await?;
    refuse_file_external(&db, &rel)?;
    let summary = drive_update(
        ctx,
        session,
        Arc::clone(&db),
        rel.clone(),
        UpdateParams {
            revision,
            depth,
            kind: UpdateKind::Update,
            allow_unversioned_obstructions: false,
            ignore_ancestry: false,
            sticky_depth: false,
        },
        cancel,
    )
    .await?;
    with_externals(ctx, &db, &rel, depth, summary, cancel).await
}

/// Re-points the working copy item at `path` to `url` (same repository) at `revision`.
pub async fn switch(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    url: &RepoUrl,
    revision: Option<Revnum>,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    let (db, rel) = open(ctx, path).await?;
    refuse_file_external(&db, &rel)?;
    let summary = drive_update(
        ctx,
        session,
        Arc::clone(&db),
        rel.clone(),
        UpdateParams {
            revision,
            depth,
            kind: UpdateKind::Switch(url.clone()),
            allow_unversioned_obstructions: false,
            ignore_ancestry: false,
            sticky_depth: false,
        },
        cancel,
    )
    .await?;
    with_externals(ctx, &db, &rel, depth, summary, cancel).await
}
