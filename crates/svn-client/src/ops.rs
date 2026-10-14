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

/// Updates the working copy item at `path` to `revision` (HEAD when `None`).
///
/// `depth` works like `svn update --depth`: `Unknown` updates to the depths recorded in the
/// working copy; a narrower depth limits the update (nodes outside it keep their revision, and
/// children the server added there are recorded as `not-present`); it is never recorded and
/// never widens the working copy. Changing the recorded depth (`--set-depth`) is not supported.
pub async fn update(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    revision: Option<Revnum>,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    let (db, rel) = open(ctx, path).await?;
    drive_update(
        ctx,
        session,
        db,
        rel,
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
    .await
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
    drive_update(
        ctx,
        session,
        db,
        rel,
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
    .await
}
