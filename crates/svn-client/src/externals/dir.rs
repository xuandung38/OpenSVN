//! Directory externals: separate working copies checked out, updated or switched in place and
//! registered in the defining working copy's `EXTERNALS` table (svn's `switch_dir_external`).

use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{Depth, NodeKind, RelPath, RepoUrl, Result, Revnum};
use svn_wc::db::{NewExternal, WcDb};

use super::{
    DirLock, ExternalAction, Item, Outcome, blocking, create_parents, in_the_way, remove, report,
};
use crate::status::is_wc_root_dir;
use crate::update::{repos_relpath_of, root_string};
use crate::wc;

/// What is at the external's path before it is fetched.
enum Existing {
    /// Nothing, or an unversioned directory to check out into.
    Nothing,
    /// A working copy of the external's repository at `relpath`.
    SameRepo { relpath: String, url: RepoUrl },
    /// A working copy of another repository.
    OtherRepo,
}

fn inspect_existing(
    owner: &svn_wc::db::LockOwner,
    abs: &std::path::Path,
    session_root: &str,
    session_uuid: &str,
) -> Result<Existing> {
    if !is_wc_root_dir(abs) {
        return Ok(Existing::Nothing);
    }
    let (db, _) = WcDb::open_and_recover(abs, owner.clone())?;
    let (root, uuid) = wc::repos_identity(&db)?;
    if uuid != session_uuid || root.trim_end_matches('/') != session_root {
        return Ok(Existing::OtherRepo);
    }
    let relpath = db
        .read_base(&RelPath::root())?
        .and_then(|n| n.repos_relpath)
        .unwrap_or_default();
    let base = RepoUrl::parse(&root)?;
    let url = if relpath.is_empty() {
        base
    } else {
        base.join(&RelPath::new(&relpath)?)?
    };
    Ok(Existing::SameRepo { relpath, url })
}

/// Checks out, updates or switches the directory external of `job` to `url` at `rev` through
/// `session` (anchored at `url`), then registers it.
pub(super) async fn ensure(
    job: &Item<'_>,
    url: &RepoUrl,
    session: Arc<dyn RaSession>,
    rev: Revnum,
    out: &mut Outcome,
) -> Result<()> {
    let abs = wc::abs_path(job.db, job.local)?;
    let wanted = repos_relpath_of(session.repos_root(), session.session_url())?;
    let (session_root, session_uuid) = (root_string(session.as_ref()), session.uuid().to_owned());
    let existing = {
        let (owner, abs, r, u) = (
            job.ctx.lock_owner.clone(),
            abs.clone(),
            session_root.clone(),
            session_uuid.clone(),
        );
        blocking(move || inspect_existing(&owner, &abs, &r, &u)).await?
    };
    let inner = job.nested_ctx();
    let mut existing = existing;
    if matches!(existing, Existing::OtherRepo) {
        // Only an external this working copy registered is replaced; any other working copy
        // in the way belongs to the user.
        let Some(prev) = job.previous.filter(|p| p.kind == NodeKind::Dir) else {
            return Err(in_the_way(format!(
                "cannot fetch the external '{}': a working copy of another repository is in the \
                 way",
                job.local
            )));
        };
        let r = remove::remove(job.ctx, job.db, prev, job.cancel).await?;
        if r.action != ExternalAction::Removed {
            return Err(in_the_way(format!(
                "the external '{}' moved to another repository, but the old working copy was \
                 kept: {}",
                job.local,
                r.message.unwrap_or_default()
            )));
        }
        existing = Existing::Nothing;
    }
    let (summary, action) = match existing {
        Existing::SameRepo { relpath, .. } if relpath == wanted => {
            let summary = crate::ops::update(
                &inner,
                session.as_ref(),
                &abs,
                Some(rev),
                Depth::Unknown,
                job.cancel,
            )
            .await?;
            (summary, ExternalAction::Updated)
        }
        Existing::SameRepo { url: current, .. } => {
            // `switch` needs a session anchored at the working copy's current URL.
            let at_current = job.opener.open(&current, job.cancel).await?;
            let summary = crate::ops::switch(
                &inner,
                at_current.as_ref(),
                &abs,
                url,
                Some(rev),
                Depth::Unknown,
                // Like svn's `switch_dir_external`: an external may point anywhere.
                true,
                job.cancel,
            )
            .await?;
            (summary, ExternalAction::Updated)
        }
        Existing::Nothing | Existing::OtherRepo => {
            if abs.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
                return Err(in_the_way(format!(
                    "cannot fetch the external '{}': a file is in the way",
                    job.local
                )));
            }
            create_parents(&abs)?;
            let summary = crate::checkout::checkout(
                &inner,
                session.as_ref(),
                &abs,
                Some(rev),
                Depth::Infinity,
                false,
                job.cancel,
            )
            .await?;
            (summary, ExternalAction::Fetched)
        }
    };
    let revision = summary.revision.or(Some(rev));
    // Register before the nested reports, in svn's notification order.
    let reg = NewExternal {
        kind: NodeKind::Dir,
        repos_root: session_root,
        repos_uuid: session_uuid,
        def_local_relpath: job.def_dir.clone(),
        def_repos_relpath: wanted,
        def_operational_revision: job.item.peg.number(),
        def_revision: job.item.revision.number(),
    };
    {
        let (db, def, local, cancel) = (
            Arc::clone(job.db),
            job.def_dir.clone(),
            job.local.clone(),
            job.cancel.clone(),
        );
        blocking(move || {
            let _lock = DirLock::obtain(&db, &def, &cancel)?;
            db.with_txn(|t| t.external_add(&local, &reg))
        })
        .await?;
    }
    out.reports
        .push(report(job.local, NodeKind::Dir, action, revision));
    out.absorb(job.local, summary)
}
