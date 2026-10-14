//! `checkout`: create a working copy and fill it with an update drive.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RelPath, Result, Revnum, SvnError};
use svn_wc::db::{NewWc, WcDb};

use crate::ctx::{ClientCtx, UpdateSummary};
use crate::update::repos_relpath_of;
use crate::update::{UpdateKind, UpdateParams, drive_update};

/// Checks out the session URL at `revision` (HEAD when `None`) into `path`.
///
/// `path` is created when missing. The working copy format is always 31. The root is recorded as
/// `incomplete` first, so a cancelled or failed checkout is resumed by `update`.
/// `allow_unversioned_obstructions` lets the checkout adopt files that already exist at `path`.
/// Externals are fetched afterwards when [`ClientCtx::externals`] is set (see
/// [`crate::externals`]); their outcome is in [`UpdateSummary::externals`].
pub async fn checkout(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    revision: Option<Revnum>,
    depth: Depth,
    allow_unversioned_obstructions: bool,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    if matches!(depth, Depth::Exclude) {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            "depth 'exclude' is not valid for a checkout",
        ));
    }
    session.ensure_authenticated(cancel).await?;
    let rev = match revision {
        Some(r) => r,
        None => session.youngest_rev(cancel).await?,
    };
    cancel.check()?;
    let new = NewWc {
        repos_root_url: crate::update::root_string(session),
        repos_uuid: session.uuid().to_owned(),
        repos_relpath: repos_relpath_of(session.repos_root(), session.session_url())?,
        revision: rev.0,
        depth: if depth == Depth::Unknown {
            Depth::Infinity
        } else {
            depth
        },
    };
    let requested = depth;
    let (path_buf, owner) = (path.to_path_buf(), ctx.lock_owner.clone());
    let db = tokio::task::spawn_blocking(move || -> Result<WcDb> {
        fs::create_dir_all(&path_buf).map_err(|e| {
            SvnError::from_io(format!("cannot create '{}'", path_buf.display()), &e)
        })?;
        let root = fs::canonicalize(&path_buf).map_err(|e| {
            SvnError::from_io(format!("cannot resolve '{}'", path_buf.display()), &e)
        })?;
        let db = WcDb::create(&root, &new)?;
        db.set_lock_owner(owner);
        Ok(db)
    })
    .await
    .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??;
    let db = Arc::new(db);
    let mut summary = drive_update(
        ctx,
        session,
        Arc::clone(&db),
        RelPath::root(),
        UpdateParams {
            revision: Some(rev),
            depth,
            kind: UpdateKind::Update,
            allow_unversioned_obstructions,
            ignore_ancestry: false,
        },
        cancel,
    )
    .await?;
    let ext = crate::externals::process(ctx, &db, &RelPath::root(), requested, cancel).await?;
    summary.events.extend(ext.events);
    summary.externals = ext.reports;
    Ok(summary)
}
