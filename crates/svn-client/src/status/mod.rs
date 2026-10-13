//! `status`: local state from wc.db and the disk, optionally compared with the repository.

mod externals;
mod ignore;
mod local;
mod remote;

use std::path::Path;
use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{CancelToken, Depth, ErrorCode, Result, SvnError, WcStatus};

pub use externals::{
    CASE_INSENSITIVE_FS, EXTERNALS_PROP, ExternalPaths, external_targets, is_wc_root_dir,
};
pub use ignore::{DEFAULT_GLOBAL_IGNORES, fnmatch};
pub use local::local_status;
pub use remote::{RemoteChange, RemoteInfo};

use crate::ctx::ClientCtx;
use crate::update::root_string;
use crate::wc;

/// What `status` reports.
#[derive(Debug, Clone)]
pub struct StatusOptions {
    /// How deep to look (`Unknown` means infinity).
    pub depth: Depth,
    /// Report unmodified nodes too (`svn status -v`).
    pub show_unmodified: bool,
    /// Report unversioned items that are not ignored.
    pub include_unversioned: bool,
    /// Report ignored items.
    pub include_ignored: bool,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            depth: Depth::Infinity,
            show_unmodified: false,
            include_unversioned: true,
            include_ignored: false,
        }
    }
}

/// Local status of the working copy item at `path`.
pub async fn status(
    ctx: &ClientCtx,
    path: &Path,
    opts: &StatusOptions,
    cancel: &CancelToken,
) -> Result<Vec<WcStatus>> {
    let (owner, p, opts, cancel, globals) = (
        ctx.lock_owner.clone(),
        path.to_path_buf(),
        opts.clone(),
        cancel.clone(),
        ctx.config.global_ignores.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let (db, rel) = wc::open_containing(&owner, &p)?;
        local_status(&db, &rel, &opts, globals.as_deref(), &cancel)
    })
    .await
    .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))?
}

/// Status with the repository side (`svn status -u`): authenticates, asks the server which paths
/// changed since the working copy revisions and attaches repository locks. No lock is taken on
/// the working copy.
pub async fn status_remote(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    opts: &StatusOptions,
    cancel: &CancelToken,
) -> Result<Vec<WcStatus>> {
    session.ensure_authenticated(cancel).await?;
    let (owner, p) = (ctx.lock_owner.clone(), path.to_path_buf());
    let (db, rel) = tokio::task::spawn_blocking(move || wc::open_containing(&owner, &p))
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??;
    let db = Arc::new(db);
    let anchor = crate::update::anchor_of(&db, session)?;
    let target = wc::strip_rel(&rel, &anchor).ok_or_else(|| {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{rel}' is not below the session URL"),
        )
    })?;
    let local = {
        let (db, rel, opts, cancel, globals) = (
            Arc::clone(&db),
            rel.clone(),
            opts.clone(),
            cancel.clone(),
            ctx.config.global_ignores.clone(),
        );
        tokio::task::spawn_blocking(move || {
            local_status(&db, &rel, &opts, globals.as_deref(), &cancel)
        })
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??
    };
    let info = remote::fetch(
        session,
        Arc::clone(&db),
        anchor,
        target,
        opts.depth,
        root_string(session),
        cancel,
    )
    .await?;
    Ok(remote::merge(local, &info))
}
