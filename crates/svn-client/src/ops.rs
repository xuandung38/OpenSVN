//! `update` and `switch` on existing working copies.

use std::path::Path;
use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RepoUrl, Result, Revnum, SvnError};

use crate::ctx::{ClientCtx, UpdateSummary};
use crate::update::{UpdateKind, UpdateParams, drive_update};
use crate::wc;

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

/// Re-points the working copy item at `path` to `url` (same repository) at `revision`.
///
/// Like `svn switch`, the target must share history with `url` (one is a branch or tag of the
/// other's line of development): otherwise the switch is refused with
/// [`ErrorCode::NoCommonAncestry`] before anything changes, unless `ignore_ancestry` is set
/// (`--ignore-ancestry`), which also lets the server treat unrelated nodes as the same.
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
    let target = switch_target(session, url)?;
    let revision = if ignore_ancestry {
        revision
    } else {
        session.ensure_authenticated(cancel).await?;
        // svn resolves HEAD first and compares the histories at that revision.
        let rev = match revision {
            Some(r) => r,
            None => session.youngest_rev(cancel).await?,
        };
        check_ancestry(session, &db, &rel, path, url, &target, rev, cancel).await?;
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

/// Repository path of the switch URL; a URL of another repository is refused like svn does
/// (`SVN_ERR_WC_INVALID_SWITCH`).
fn switch_target(session: &dyn RaSession, url: &RepoUrl) -> Result<svn_types::RelPath> {
    let not_same = || {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "'{url}' is not in the same repository as the working copy ('{}')",
                session.repos_root()
            ),
        )
        .with_svn_code(155_025)
    };
    let rel = crate::update::repos_relpath_of(session.repos_root(), url).map_err(|_| not_same())?;
    svn_types::RelPath::new(&rel).map_err(|_| not_same())
}

/// svn's ancestry check: the BASE of the target and `url@rev` must have a common ancestor.
#[allow(clippy::too_many_arguments)]
async fn check_ancestry(
    session: &dyn RaSession,
    db: &svn_wc::db::WcDb,
    rel: &svn_types::RelPath,
    path: &Path,
    url: &RepoUrl,
    target: &svn_types::RelPath,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<()> {
    let base = db
        .read_base(rel)?
        .filter(|n| n.presence.is_present())
        .and_then(|n| Some((n.repos_relpath?, n.revision?)));
    let related = match base {
        Some((repos, base_rev)) => {
            let repos = svn_types::RelPath::new(&repos)?;
            svn_ra_api::have_common_ancestor(
                session,
                (target, rev),
                (&repos, Revnum(base_rev)),
                cancel,
            )
            .await?
        }
        None => false,
    };
    if related {
        return Ok(());
    }
    Err(SvnError::new(
        ErrorCode::NoCommonAncestry,
        format!(
            "'{}' does not share common version control ancestry with '{url}': it is not a \
             branch or tag of the same line of development. Switch to a branch or tag copied \
             from it, or switch with ancestry ignored",
            path.display()
        ),
    )
    .with_svn_code(195_012))
}
