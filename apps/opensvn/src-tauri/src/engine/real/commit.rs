//! Operations that send working copy changes to the repository through `svn-client`: commit and
//! copying a working copy path to a URL.
//!
//! A commit runs in one call of the working copy's actor job:
//!
//! 1. A session is opened at the working copy URL ([`ConnectOptions::at_url`]: the connector
//!    reparents its cached root session there) and authenticates before any working copy lock
//!    is taken. Like `svn commit`, the commit is driven relative to that URL
//!    (`CommitOptions::session_prefix`), so a user whose path-based authorization only covers
//!    the working copy's subtree (no access to the repository root) can commit.
//! 2. The registered root is opened with `svn_client::wc::open_containing`, which gives the
//!    canonical root spelling crash recovery uses too, so the commit journal is found again on
//!    the next open whatever path the working copy was registered under.
//! 3. With `auto_add_remove`, under the working copy lock: unversioned items in the selection
//!    are scheduled for addition (never conflict marker files, externals or nested working
//!    copies) and missing versioned items are scheduled for deletion (never the content of a
//!    folder an interrupted checkout or update left incomplete, issue #99). A conflict in the
//!    selection fails the commit before anything is scheduled.
//! 4. `svn_client::commit::commit` sends every selected path recursively (the fake engine's
//!    selection semantics), journaled below [`RealEngine::commit_journal_dir`], with the user
//!    name the session authenticated as recorded for recovery.
//!
//! When the server's answer to the final `MERGE` is lost, the journal is settled once right away
//! over an unattended session ([`svn_client::commit::recover_wc_commits`]): a commit found in the
//! log is finalized and reported as committed; otherwise the non-retryable
//! `commit-outcome-unknown` error is returned and the journal stays for recovery on open.
//!
//! The scheduled additions and deletions stay when the commit then fails, as in TortoiseSVN.
//!
//! Copying a working copy (with its local changes) to a URL needs a working-copy-to-URL copy in
//! `svn-client`, which does not exist yet; it fails with a user-facing error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use opensvn_ipc::{CommitRequest, CopyLocationRequest, Rev};
use svn_client::StatusOptions;
use svn_client::commit::journal::JournalStore;
use svn_client::commit::{CommitContext, CommitOptions, CommitResult};
use svn_client::local::{self as client_local, AddOptions, DeleteOptions};
use svn_client::status::{is_wc_root_dir, local_status};
use svn_ra_api::RaSession;
use svn_types::{Depth, ErrorCode, NodeStatus, PropStatus, RelPath, RepoUrl, SvnError, safe_join};
use svn_wc::WcDb;

use super::local_util::{self, LocalEnv};
use super::wc_read::{self, blocking};
use super::{ConnectOptions, RealEngine};
use crate::engine::context::{OpContext, ProgressUpdate};
use crate::engine::facade::{EngineResult, WcRef};
use crate::engine::text::percent_decode;

/// Progress phase of a commit.
pub(super) const PHASE_COMMIT: &str = "commit";
/// Progress phase reported when the commit went through but the working copy needs cleanup
/// (`current_path` carries the reason).
pub(super) const PHASE_NEEDS_CLEANUP: &str = "needs-cleanup";

/// Repository path of the session URL below the repository root (`""` at the root), decoded.
fn session_prefix(session: &dyn RaSession) -> EngineResult<String> {
    let root = session.repos_root().to_string();
    let root = root.trim_end_matches('/');
    let url = session.session_url().to_string();
    let url = url.trim_end_matches('/');
    if url == root {
        return Ok(String::new());
    }
    url.strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(percent_decode)
        .ok_or_else(|| {
            SvnError::new(
                ErrorCode::Internal,
                format!("session {url} is not inside repository {root}"),
            )
        })
}

/// Error of a commit with nothing to send (the fake engine's wording).
fn nothing_to_commit() -> SvnError {
    SvnError::new(ErrorCode::Internal, "nothing to commit")
}

fn push_unique(list: &mut Vec<RelPath>, rel: RelPath) {
    if !list.contains(&rel) {
        list.push(rel);
    }
}

/// Schedules the unversioned items of the selection for addition and its missing items for
/// deletion; the caller holds the working copy lock. Returns the directories that were added
/// only as parents of a selected item: they must be committed too.
///
/// Fails with `conflict-blocks-commit` before changing anything when a selected item is in
/// conflict, as the commit would.
fn auto_add_remove(db: &WcDb, env: &LocalEnv, rels: &[RelPath]) -> EngineResult<Vec<RelPath>> {
    let markers = local_util::conflict_markers(db)?;
    let mut add: Vec<RelPath> = Vec::new();
    let mut delete: Vec<RelPath> = Vec::new();
    let opts = StatusOptions {
        depth: Depth::Infinity,
        show_unmodified: false,
        include_unversioned: true,
        include_ignored: false,
    };
    for rel in rels {
        env.cancel.check()?;
        if db.read_node(rel)?.is_none() {
            // A selected unversioned item is added as svn adds an explicit target (even when
            // it matches an ignore pattern); externals and nested working copies are refused.
            let items = local_util::unversioned_items(db, env, rel, true, false)?;
            if items.is_empty() && safe_join(db.wcroot(), rel)?.symlink_metadata().is_err() {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("'{rel}' is not under version control and does not exist"),
                ));
            }
            for item in items {
                if !markers.contains(&item) {
                    push_unique(&mut add, item);
                }
            }
            continue;
        }
        for s in local_status(db, rel, &opts, Some(env.global_ignores()), &env.cancel)? {
            if s.text == NodeStatus::Conflicted
                || s.props == PropStatus::Conflicted
                || s.tree_conflict
            {
                return Err(SvnError::new(
                    ErrorCode::ConflictBlocksCommit,
                    format!("aborting commit: '{}' remains in conflict", s.path),
                ));
            }
            match s.text {
                NodeStatus::Unversioned
                    if !markers.contains(&s.path)
                        && !is_wc_root_dir(&safe_join(db.wcroot(), &s.path)?) =>
                {
                    push_unique(&mut add, s.path);
                }
                NodeStatus::Missing if local_util::missing_is_deletable(db, &s.path)? => {
                    push_unique(&mut delete, s.path);
                }
                _ => {}
            }
        }
    }
    // Deleting a missing directory deletes what was below it.
    let roots: Vec<RelPath> = delete
        .iter()
        .filter(|d| !delete.iter().any(|o| o != *d && d.starts_with(o)))
        .cloned()
        .collect();
    let abs = |list: &[RelPath]| -> EngineResult<Vec<PathBuf>> {
        list.iter().map(|r| safe_join(db.wcroot(), r)).collect()
    };
    if !roots.is_empty() {
        let opts = DeleteOptions {
            // Nothing of a missing node is on disk; keeping local files means a file that
            // reappeared in the meantime is never removed.
            keep_local: true,
            force: false,
            global_ignores: env.global_ignores().to_vec(),
        };
        client_local::delete(db, &env.cancel, &abs(&roots)?, &opts)?;
    }
    if add.is_empty() {
        return Ok(Vec::new());
    }
    let opts = AddOptions {
        depth: Depth::Infinity,
        add_parents: true,
        ..AddOptions::default()
    };
    let added = client_local::add(db, &env.cancel, &abs(&add)?, &opts, &env.add)?;
    // Parents added on the way are new directories outside the selection: the commit must
    // include them, or the server is asked to add an item into a directory it does not have.
    let mut parents = Vec::new();
    for item in &add {
        let mut cur = item.parent();
        while let Some(p) = cur {
            if added.contains(&p) && !rels.iter().any(|r| p.starts_with(r)) {
                push_unique(&mut parents, p.clone());
            }
            cur = p.parent();
        }
    }
    Ok(parents)
}

/// Ids of the journals of the working copy at `db` whose `MERGE` outcome is unknown.
fn unknown_outcome_journals(dir: &Path, db: &WcDb) -> EngineResult<Vec<String>> {
    let store = JournalStore::new(dir);
    let mut ids = Vec::new();
    for path in store.list()? {
        if let Ok(data) = JournalStore::load(&path)
            && data.wcroot == db.wcroot()
            && data.outcome_unknown()
        {
            ids.push(data.id);
        }
    }
    Ok(ids)
}

impl RealEngine {
    pub(super) async fn do_commit(
        &self,
        wc: &WcRef,
        req: &CommitRequest,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        ctx.check()?;
        let info = wc_read::wc_info(&wc.root, ctx).await?;
        let url = RepoUrl::parse(&info.url)?;
        let session = self
            .connector
            .connect_with(&url, &ConnectOptions::at_url(ctx), &ctx.cancel)
            .await?;
        // Any prompt happens now, before the working copy is locked.
        session.ensure_authenticated(&ctx.cancel).await?;
        let prefix = session_prefix(session.as_ref())?;
        let author = session.authenticated_user();

        let client = self.client_ctx(ctx);
        let config = self.svn_config.clone();
        let (root, paths, auto) = (wc.root.clone(), req.paths.clone(), req.auto_add_remove);
        let cancel = ctx.cancel.clone();
        let (db, targets) = blocking(move || {
            let db = local_util::open_wc_root(&client, &root)?;
            let env = local_util::local_env(&config, &db, cancel)?;
            let mut rels = Vec::with_capacity(paths.len());
            for p in &paths {
                push_unique(&mut rels, local_util::target(&db, p)?.0);
            }
            if auto && !rels.is_empty() {
                let parents = client_local::lock::with_wc_lock(&db, &env.cancel, || {
                    auto_add_remove(&db, &env, &rels)
                })?;
                for p in parents {
                    push_unique(&mut rels, p);
                }
                // A selected item that stays unversioned (a conflict marker file) is not sent.
                let mut versioned = Vec::with_capacity(rels.len());
                for r in rels {
                    if db.read_node(&r)?.is_some() {
                        versioned.push(r);
                    }
                }
                rels = versioned;
            }
            let targets = rels
                .iter()
                .map(|r| safe_join(db.wcroot(), r))
                .collect::<EngineResult<Vec<_>>>()?;
            Ok((Arc::new(db), targets))
        })
        .await?;
        if targets.is_empty() {
            return Err(nothing_to_commit());
        }

        ctx.progress.report(ProgressUpdate {
            phase: PHASE_COMMIT.to_owned(),
            total: Some(targets.len() as u64),
            ..ProgressUpdate::default()
        });
        let opts = CommitOptions {
            depth: Depth::Infinity,
            keep_locks: req.keep_locks,
            session_prefix: prefix,
            author,
            ..CommitOptions::new(req.message.clone())
        };
        let state_dir = self.commit_journal_dir();
        let result = svn_client::commit::commit(
            &db,
            &CommitContext {
                ra: session.as_ref(),
                state_dir: &state_dir,
                cancel: &ctx.cancel,
            },
            &targets,
            &opts,
        )
        .await;
        match result {
            Ok(Some(done)) => Ok(committed(&done, ctx)),
            Ok(None) => Err(nothing_to_commit()),
            Err(e) if e.code == ErrorCode::CommitOutcomeUnknown => {
                self.settle_unknown_outcome(&db, &info.repo_root_url, e, ctx)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    /// The server's answer to the final `MERGE` was lost: settles the commit's journal once,
    /// unattended. Returns the revision when the repository shows the commit; otherwise `err`
    /// (non-retryable), or a retryable error when the commit certainly did not happen.
    async fn settle_unknown_outcome(
        &self,
        db: &Arc<WcDb>,
        repo_root_url: &str,
        err: SvnError,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        let dir = self.commit_journal_dir();
        // No commit starts while a journal of this working copy has an unknown outcome, so the
        // one found now is this commit's.
        let ours = {
            let (dir, db) = (dir.clone(), db.clone());
            blocking(move || unknown_outcome_journals(&dir, &db)).await
        };
        let id = match ours {
            Ok(ids) if ids.len() == 1 => ids.into_iter().next(),
            Ok(ids) => {
                tracing::warn!(
                    journals = ids.len(),
                    "cannot tell which journal is this commit's"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "commit journals could not be listed");
                None
            }
        };
        let Some(id) = id else {
            return Err(err);
        };
        let session = match self.unattended_session(repo_root_url, ctx).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "no repository session to settle the commit");
                None
            }
        };
        let outcome =
            svn_client::commit::recover_wc_commits(&dir, db, session.as_deref(), &ctx.cancel).await;
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, "settling the commit failed");
                return Err(err);
            }
        };
        if let Some((_, rev)) = outcome.finalized_revisions.iter().find(|(j, _)| *j == id) {
            tracing::info!(rev, journal = %id, "commit with a lost MERGE answer found in the log");
            return Ok(Rev(*rev));
        }
        if outcome.aborted.contains(&id) {
            return Err(SvnError::new(
                ErrorCode::Network,
                "the connection was lost while the commit finished and the server did not apply \
                 it; nothing was committed and the working copy is unchanged, so the commit can \
                 be repeated",
            )
            .with_child(err));
        }
        let reason = outcome
            .needs_user
            .iter()
            .chain(outcome.failed.iter())
            .find(|(j, _)| *j == id)
            .map(|(_, e)| e.clone());
        Err(match reason {
            Some(reason) => err.with_child(reason),
            None => err,
        })
    }

    pub(super) async fn do_copy_location_from_wc(
        &self,
        _req: &CopyLocationRequest,
        _wc: &WcRef,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        ctx.check()?;
        Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            "Copying a working copy with its local changes to the repository is not supported \
             yet. Commit the changes first, then copy the repository location instead.",
        ))
    }
}

/// Reports what a successful commit left to do and returns its revision.
fn committed(done: &CommitResult, ctx: &OpContext) -> Rev {
    let rev = done.revision.0;
    if let Some(hook) = &done.post_commit_err {
        tracing::warn!(rev, warning = %hook, "post-commit hook reported a problem");
    }
    if let Some(warning) = &done.wc_warning {
        tracing::warn!(rev, %warning, "committed, but the working copy needs cleanup");
        ctx.progress.report(ProgressUpdate {
            phase: PHASE_NEEDS_CLEANUP.to_owned(),
            current_path: Some(warning.clone()),
            ..ProgressUpdate::default()
        });
    }
    Rev(rev)
}
