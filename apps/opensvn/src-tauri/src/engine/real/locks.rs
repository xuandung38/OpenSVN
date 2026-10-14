//! Repository locks (`lock` / `unlock`). Locking URLs only talks to the server; locking working
//! copy paths also records the token in the `LOCK` table of `wc.db`, exactly where libsvn_wc
//! keeps it, so the path shows as locked-by-me and a later commit or unlock presents the token.
//! Like `svn lock`, the working copy write lock (`WC_LOCK`) of the node's parent directory is held
//! for the whole operation, so another client cannot change the same directory meanwhile and a
//! directory locked by someone else fails with [`ErrorCode::WcBusy`] before the server is asked.
//! Like libsvn_wc, a `svn:needs-lock` file becomes writable once its token is recorded and
//! read-only again once it is forgotten.

use std::path::Path;
use std::sync::Arc;

use opensvn_ipc::LockConflict;
use opensvn_ipc::time_fmt::timestamp_to_iso;
use svn_ra_api::RaSession;
use svn_types::{Depth, ErrorCode, RelPath, Revnum, SvnError};
use svn_wc::WcDb;
use svn_wc::db::DbWorkingCopyOps;
use svn_wc::db::ObtainMode;
use svn_wc::translate::flags;
use svn_wc::workqueue::WorkingCopyOps;

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
                    // Sign in before the directory is locked: no prompt while a lock is held.
                    let (session, path) = self
                        .locate_repo_path(&loc.repo_root, &loc.repos_path, ctx)
                        .await?;
                    let held = self.lock_parent_dir(&wc.root, rel).await?;
                    let done = async {
                        let lock = session
                            .lock(
                                &path,
                                comment,
                                steal,
                                Some(Revnum(loc.revision)),
                                &ctx.cancel,
                            )
                            .await?;
                        if let Err(e) = record_lock(&held.db, &wc.root, &loc, &lock, rel).await {
                            // The server lock exists but the working copy cannot remember its
                            // token: give it back instead of leaving one nobody can release.
                            release_quietly(session.as_ref(), &path, &lock.token).await;
                            return Err(e);
                        }
                        Ok(())
                    }
                    .await;
                    finish(done, held.release().await)?;
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
                    let (session, path) = self
                        .locate_repo_path(&loc.repo_root, &loc.repos_path, ctx)
                        .await?;
                    let held = self.lock_parent_dir(&wc.root, rel).await?;
                    let done = async {
                        let token = recorded_token(&held.db, &loc).await?;
                        session
                            .unlock(&path, token.as_deref(), break_lock, &ctx.cancel)
                            .await?;
                        forget_lock(&held.db, &wc.root, &loc, rel).await
                    }
                    .await;
                    finish(done, held.release().await)?;
                }
                Ok(())
            }
        }
    }
}

impl RealEngine {
    pub(super) async fn do_lock_conflicts(
        &self,
        scope: &LockScope,
        ctx: &OpContext,
    ) -> EngineResult<Vec<LockConflict>> {
        let mut out = Vec::new();
        match scope {
            LockScope::Urls(urls) => {
                for url in urls {
                    let (session, path) = self.locate(url, ctx).await?;
                    for l in session.get_locks(&path, Depth::Empty, &ctx.cancel).await? {
                        out.push(conflict(url, l));
                    }
                }
            }
            LockScope::Wc { wc, paths } => {
                for rel in paths {
                    ctx.check()?;
                    let loc = node_location(&wc.root, rel).await?;
                    let (session, path) = self
                        .locate_repo_path(&loc.repo_root, &loc.repos_path, ctx)
                        .await?;
                    let ours = {
                        let (root, loc) = (wc.root.clone(), loc.clone());
                        blocking(move || recorded_token_in(&open_db(&root)?, &loc)).await?
                    };
                    for l in session.get_locks(&path, Depth::Empty, &ctx.cancel).await? {
                        if ours.as_deref() != Some(l.token.as_str()) {
                            out.push(conflict(rel, l));
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// IPC description of a lock someone else holds on the requested `path`; the token stays here.
fn conflict(path: &str, l: svn_types::LockInfo) -> LockConflict {
    LockConflict {
        path: path.to_owned(),
        owner: l.owner,
        comment: l.comment,
        created_at: Some(timestamp_to_iso(l.created)),
    }
}

/// Working copy write lock on one directory, held by an open handle. Released by
/// [`HeldDir::release`]; when the operation is cancelled or its future dropped before that, the
/// guard releases the lock on drop, so an abandoned lock/unlock never leaves the directory locked.
struct HeldDir {
    db: Arc<WcDb>,
    dir: RelPath,
    /// Still to be released (cleared by `release`).
    armed: bool,
}

impl HeldDir {
    async fn release(mut self) -> EngineResult<()> {
        self.armed = false;
        let (db, dir) = (self.db.clone(), self.dir.clone());
        blocking(move || db.wc_lock_release(&dir)).await
    }
}

impl Drop for HeldDir {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A short local database write; run in place so the lock is gone when the drop returns.
        if let Err(e) = self.db.wc_lock_release(&self.dir) {
            tracing::warn!(dir = %self.dir, error = %e, "could not release an abandoned lock");
        }
    }
}

/// The operation's result, or the release error when only the release failed.
fn finish(done: EngineResult<()>, released: EngineResult<()>) -> EngineResult<()> {
    match (done, released) {
        (Err(e), Err(r)) => {
            tracing::warn!(error = %r, "could not release the working copy lock");
            Err(e)
        }
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

impl RealEngine {
    /// Takes the working copy write lock of the directory containing `rel` (levels 0).
    async fn lock_parent_dir(&self, root: &Path, rel: &str) -> EngineResult<HeldDir> {
        let dir = RelPath::new(rel)?.parent().unwrap_or_else(RelPath::root);
        let (root, owner) = (root.to_path_buf(), self.lock_owner());
        blocking(move || {
            let db = Arc::new(open_db(&root)?);
            db.set_lock_owner(owner);
            db.wc_lock_obtain(&dir, 0, ObtainMode::Normal)?;
            Ok(HeldDir {
                db,
                dir,
                armed: true,
            })
        })
        .await
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
    db: &Arc<WcDb>,
    root: &Path,
    loc: &NodeLocation,
    lock: &svn_types::LockInfo,
    rel: &str,
) -> EngineResult<()> {
    let (db, loc, lock) = (db.clone(), loc.clone(), lock.clone());
    let (root, rel) = (root.to_path_buf(), RelPath::new(rel)?);
    blocking(move || {
        db.with_write(|tx| {
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
        })?;
        sync_flags(&db, &root, &rel);
        Ok(())
    })
    .await
}

async fn recorded_token(db: &Arc<WcDb>, loc: &NodeLocation) -> EngineResult<Option<String>> {
    let (db, loc) = (db.clone(), loc.clone());
    blocking(move || recorded_token_in(&db, &loc)).await
}

/// Token of the lock `wc.db` records for the node at `loc`.
fn recorded_token_in(db: &WcDb, loc: &NodeLocation) -> EngineResult<Option<String>> {
    use rusqlite::OptionalExtension;
    db.with_read(|c| {
        c.query_row(
            "SELECT lock_token FROM lock WHERE repos_id = ?1 AND repos_relpath = ?2",
            (loc.repos_id, loc.repos_path.as_str()),
            |r| r.get(0),
        )
        .optional()
        .map_err(svn_wc::db::sql_err)
    })
}

async fn forget_lock(
    db: &Arc<WcDb>,
    root: &Path,
    loc: &NodeLocation,
    rel: &str,
) -> EngineResult<()> {
    let (db, loc) = (db.clone(), loc.clone());
    let (root, rel) = (root.to_path_buf(), RelPath::new(rel)?);
    blocking(move || {
        db.with_write(|tx| {
            tx.execute(
                "DELETE FROM lock WHERE repos_id = ?1 AND repos_relpath = ?2",
                (loc.repos_id, loc.repos_path.as_str()),
            )
            .map(|_| ())
            .map_err(svn_wc::db::sql_err)
        })?;
        sync_flags(&db, &root, &rel);
        Ok(())
    })
    .await
}

/// Makes the permissions of the file `rel` follow its lock and `svn:needs-lock`, as the
/// `sync-file-flags` work item does, but directly: the lock change is already durable and stays
/// the outcome, so a failure is only logged and never leaves queued work behind (which would
/// block the working copy until a cleanup). Directories and files missing on disk are skipped.
fn sync_flags(db: &WcDb, root: &Path, rel: &RelPath) {
    let abs = root.join(rel.as_str());
    if !std::fs::symlink_metadata(&abs).is_ok_and(|m| m.is_file()) {
        return;
    }
    let done = match DbWorkingCopyOps::new(db).sync_flags_input(rel.as_str()) {
        Ok(Some(input)) => flags::sync_with_props(&abs, input).map_err(|e| e.to_string()),
        Ok(None) => Ok(()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = done {
        tracing::warn!(%rel, error = %e, "could not update the permissions of a locked file");
    }
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
