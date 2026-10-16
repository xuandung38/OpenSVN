//! `update` and `switch` on existing working copies.

use std::path::Path;
use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RelPath, RepoUrl, Result, Revnum, SvnError};
use svn_wc::db::Presence;

use crate::ancestry::{Location, youngest_common_ancestor};
use crate::ctx::{ClientCtx, UpdateSummary};
use crate::update::{UpdateKind, UpdateParams, drive_update, repos_relpath_of};
use crate::wc;

/// Subversion's `SVN_ERR_CLIENT_UNRELATED_RESOURCES`.
pub const SVN_ERR_CLIENT_UNRELATED_RESOURCES: u32 = 195_012;

/// Resolves `path` like svn does (lexically, see [`wc::open_target`]): a target that is deleted
/// or missing on disk can still be updated.
async fn open(ctx: &ClientCtx, path: &Path) -> Result<(Arc<svn_wc::db::WcDb>, svn_types::RelPath)> {
    let (owner, path) = (ctx.lock_owner.clone(), path.to_path_buf());
    let (db, rel) = tokio::task::spawn_blocking(move || wc::open_target(&owner, &path))
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

/// Re-points the working copy item at `path` to `url` (same repository) at `revision` (HEAD
/// when `None`).
///
/// Like `svn switch`, a target that shares no history with the item's BASE location is refused
/// with [`ErrorCode::UnrelatedResources`] (svn code 195012) before anything changes: the server
/// would replace the item with the unrelated tree, turning local changes into tree conflicts.
/// `ignore_ancestry` (`svn switch --ignore-ancestry`) skips that check; externals use it, as
/// svn does when it re-points an external.
#[allow(clippy::too_many_arguments)]
pub async fn switch(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    url: &RepoUrl,
    revision: Option<Revnum>,
    depth: Depth,
    ignore_ancestry: bool,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    let (db, rel) = open(ctx, path).await?;
    refuse_file_external(&db, &rel)?;
    let revision = if ignore_ancestry {
        revision
    } else {
        // The check and the drive look at the same revision, even when a commit lands between.
        session.ensure_authenticated(cancel).await?;
        let rev = match revision {
            Some(r) => r,
            None => session.youngest_rev(cancel).await?,
        };
        check_ancestry(session, &db, &rel, path, url, rev, cancel).await?;
        Some(rev)
    };
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
            ignore_ancestry,
            sticky_depth: false,
        },
        cancel,
    )
    .await?;
    with_externals(ctx, &db, &rel, depth, summary, cancel).await
}

/// Refuses `url`@`rev` unless it shares history with the BASE location of `rel`
/// (`svn_client__switch_internal`). An item without a BASE location (not in the repository)
/// shares nothing.
async fn check_ancestry(
    session: &dyn RaSession,
    db: &svn_wc::db::WcDb,
    rel: &RelPath,
    path: &Path,
    url: &RepoUrl,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<()> {
    let base = db
        .read_base(rel)?
        .filter(|n| matches!(n.presence, Presence::Normal | Presence::Incomplete))
        .and_then(|n| Some((n.repos_relpath?, n.revision?)));
    let related = match base {
        None => false,
        Some((relpath, base_rev)) => {
            let target = Location {
                relpath: RelPath::new(&repos_relpath_of(session.repos_root(), url)?)?,
                rev,
            };
            let here = Location {
                relpath: RelPath::new(&relpath)?,
                rev: Revnum(base_rev),
            };
            youngest_common_ancestor(session, &target, &here, cancel)
                .await?
                .is_some()
        }
    };
    if related {
        return Ok(());
    }
    Err(SvnError::new(
        ErrorCode::UnrelatedResources,
        format!(
            "'{url}' shares no common ancestry with '{}'",
            path.display()
        ),
    )
    .with_svn_code(SVN_ERR_CLIENT_UNRELATED_RESOURCES))
}
