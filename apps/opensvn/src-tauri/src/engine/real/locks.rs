//! Repository locks (`lock` / `unlock`). Locking URLs only talks to the server; locking working
//! copy paths also records the token in the `LOCK` table of `wc.db`, exactly where libsvn_wc
//! keeps it, so the path shows as locked-by-me and a later commit or unlock presents the token.

use std::path::Path;

use svn_ra_api::RaSession;
use svn_types::{ErrorCode, RelPath, Revnum, SvnError};

use super::RealEngine;
use super::wc_read::{NodeLocation, blocking, node_location, open_db};
use crate::engine::context::OpContext;
use crate::engine::facade::{EngineResult, LockScope};

impl RealEngine {
    pub(super) async fn do_lock(
        &self,
        scope: &LockScope,
        comment: Option<&str>,
        steal: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        match scope {
            LockScope::Urls(urls) => {
                for url in urls {
                    let (session, path) = self.locate(url, ctx).await?;
                    session
                        .lock(&path, comment, steal, None, &ctx.cancel)
                        .await?;
                }
                Ok(())
            }
            LockScope::Wc { wc, paths } => {
                ensure_writable(&wc.root).await?;
                for rel in paths {
                    ctx.check()?;
                    let loc = node_location(&wc.root, rel).await?;
                    let (session, path) = self
                        .locate_repo_path(&loc.repo_root, &loc.repos_path, ctx)
                        .await?;
                    let lock = session
                        .lock(
                            &path,
                            comment,
                            steal,
                            Some(Revnum(loc.revision)),
                            &ctx.cancel,
                        )
                        .await?;
                    if let Err(e) = record_lock(&wc.root, &loc, &lock).await {
                        // The server lock exists but the working copy cannot remember its token:
                        // give the lock back instead of leaving one nobody can release.
                        release_quietly(session.as_ref(), &path, &lock.token).await;
                        return Err(e);
                    }
                }
                Ok(())
            }
        }
    }

    pub(super) async fn do_unlock(
        &self,
        scope: &LockScope,
        break_lock: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        match scope {
            LockScope::Urls(urls) => {
                for url in urls {
                    let (session, path) = self.locate(url, ctx).await?;
                    session.unlock(&path, None, break_lock, &ctx.cancel).await?;
                }
                Ok(())
            }
            LockScope::Wc { wc, paths } => {
                ensure_writable(&wc.root).await?;
                for rel in paths {
                    ctx.check()?;
                    let loc = node_location(&wc.root, rel).await?;
                    let token = recorded_token(&wc.root, &loc).await?;
                    let (session, path) = self
                        .locate_repo_path(&loc.repo_root, &loc.repos_path, ctx)
                        .await?;
                    session
                        .unlock(&path, token.as_deref(), break_lock, &ctx.cancel)
                        .await?;
                    forget_lock(&wc.root, &loc).await?;
                }
                Ok(())
            }
        }
    }
}

/// Fails before any server call when `wc.db` cannot take the lock record (format 32).
async fn ensure_writable(root: &Path) -> EngineResult<()> {
    let root = root.to_path_buf();
    blocking(move || {
        if open_db(&root)?.is_writable() {
            Ok(())
        } else {
            Err(SvnError::new(
                ErrorCode::WcFormatUnsupported,
                "this working copy format is read-only in this version; locks cannot be recorded",
            ))
        }
    })
    .await
}

async fn record_lock(
    root: &Path,
    loc: &NodeLocation,
    lock: &svn_types::LockInfo,
) -> EngineResult<()> {
    let (root, loc, lock) = (root.to_path_buf(), loc.clone(), lock.clone());
    blocking(move || {
        open_db(&root)?.with_write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO lock \
                 (repos_id, repos_relpath, lock_token, lock_owner, lock_comment, lock_date) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    loc.repos_id,
                    loc.repos_path.as_str(),
                    &lock.token,
                    &lock.owner,
                    &lock.comment,
                    lock.created.0,
                ),
            )
            .map(|_| ())
            .map_err(svn_wc::db::sql_err)
        })
    })
    .await
}

async fn recorded_token(root: &Path, loc: &NodeLocation) -> EngineResult<Option<String>> {
    use rusqlite::OptionalExtension;
    let (root, loc) = (root.to_path_buf(), loc.clone());
    blocking(move || {
        open_db(&root)?.with_read(|c| {
            c.query_row(
                "SELECT lock_token FROM lock WHERE repos_id = ?1 AND repos_relpath = ?2",
                (loc.repos_id, loc.repos_path.as_str()),
                |r| r.get(0),
            )
            .optional()
            .map_err(svn_wc::db::sql_err)
        })
    })
    .await
}

async fn forget_lock(root: &Path, loc: &NodeLocation) -> EngineResult<()> {
    let (root, loc) = (root.to_path_buf(), loc.clone());
    blocking(move || {
        open_db(&root)?.with_write(|tx| {
            tx.execute(
                "DELETE FROM lock WHERE repos_id = ?1 AND repos_relpath = ?2",
                (loc.repos_id, loc.repos_path.as_str()),
            )
            .map(|_| ())
            .map_err(svn_wc::db::sql_err)
        })
    })
    .await
}

/// Best-effort release of a lock taken a moment ago; failure is logged, the caller already has
/// an error to report.
async fn release_quietly(session: &dyn RaSession, path: &RelPath, token: &str) {
    // A fresh token: cancellation of the user's operation must not stop the compensation.
    let cancel = svn_types::CancelToken::new();
    if let Err(e) = session.unlock(path, Some(token), false, &cancel).await {
        tracing::warn!(%path, error = %e, "could not release a lock that was not recorded");
    }
}
