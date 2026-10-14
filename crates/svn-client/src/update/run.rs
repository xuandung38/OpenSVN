//! Orchestration of one update, switch or checkout drive.

use std::sync::{Arc, PoisonError};

use svn_ra_api::{RaSession, UpdateMode, UpdateRequest};
use svn_types::{
    CancelToken, Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Result, Revnum, SvnError,
};
use svn_wc::db::{ObtainMode, WcDb};

use super::editor::UpdateEditor;
use super::reporter::build_report;
use super::restore;
use super::state::UpdateSetup;
use crate::ctx::{ClientCtx, UpdateAction, UpdateEvent, UpdateSummary};
use crate::wc;

/// What the drive does to the target.
#[derive(Debug, Clone)]
pub enum UpdateKind {
    /// Bring the target to the requested revision.
    Update,
    /// Re-point the target to another URL of the same repository.
    Switch(RepoUrl),
}

/// Parameters of [`drive_update`].
#[derive(Debug, Clone)]
pub struct UpdateParams {
    /// Target revision (`None` = HEAD).
    pub revision: Option<Revnum>,
    /// Requested depth, `svn update --depth` semantics (`Unknown`: the recorded depths); never
    /// recorded. A checkout records its depth when it creates the working copy.
    pub depth: Depth,
    /// Update or switch.
    pub kind: UpdateKind,
    /// Adopt unversioned files that are in the way (checkout into an existing tree).
    pub allow_unversioned_obstructions: bool,
    /// Ignore node ancestry (switch only).
    pub ignore_ancestry: bool,
    /// `svn update --set-depth`: record `depth` and let the drive widen the working copy to it.
    /// The caller crops first when the depth shrinks.
    pub sticky_depth: bool,
}

/// Releases the working copy lock when dropped, also on error and cancellation.
struct LockGuard {
    db: Arc<WcDb>,
    dir: RelPath,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Err(e) = self.db.wc_lock_release(&self.dir) {
            tracing::warn!("could not release working copy lock on '{}': {e}", self.dir);
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Path of `url` below `root`, percent-decoded, without leading or trailing slash.
pub fn repos_relpath_of(root: &RepoUrl, url: &RepoUrl) -> Result<String> {
    let base = root.as_url().path().trim_end_matches('/');
    let full = url.as_url().path().trim_end_matches('/');
    let rest = full.strip_prefix(base).ok_or_else(|| {
        SvnError::new(
            ErrorCode::Internal,
            format!("'{url}' is not below the repository root '{root}'"),
        )
    })?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!("'{url}' is not below the repository root '{root}'"),
        ));
    }
    Ok(percent_decode(rest.trim_matches('/')))
}

/// Root URL string the way wc.db stores it.
pub fn root_string(session: &dyn RaSession) -> String {
    session
        .repos_root()
        .to_string()
        .trim_end_matches('/')
        .to_owned()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))?
}

/// Working copy path of the directory the session URL points to.
pub fn anchor_of(db: &WcDb, session: &dyn RaSession) -> Result<RelPath> {
    let (root, uuid) = wc::repos_identity(db)?;
    if uuid != session.uuid() || root.trim_end_matches('/') != root_string(session) {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "the working copy belongs to '{root}' ({uuid}), not to '{}'",
                session.repos_root()
            ),
        ));
    }
    let wc_root = db
        .read_base(&RelPath::root())?
        .and_then(|n| n.repos_relpath)
        .unwrap_or_default();
    let session_rel = repos_relpath_of(session.repos_root(), session.session_url())?;
    let anchor = if session_rel == wc_root {
        String::new()
    } else if wc_root.is_empty() {
        session_rel.clone()
    } else if let Some(rest) = session_rel.strip_prefix(&format!("{wc_root}/")) {
        rest.to_owned()
    } else {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "the session URL '{}' is outside the working copy",
                session.session_url()
            ),
        ));
    };
    RelPath::new(&anchor)
}

/// Runs one update, switch or checkout of `target_wc` (path relative to the working copy root).
///
/// Authenticates first, then takes the working copy lock, runs leftover work items, reports,
/// drives the editor and releases the lock. On failure or cancellation the queued work of nodes
/// that were already committed is completed before the error is returned.
pub async fn drive_update(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    db: Arc<WcDb>,
    target_wc: RelPath,
    params: UpdateParams,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    session.ensure_authenticated(cancel).await?;
    cancel.check()?;

    let node = db.read_base(&target_wc)?.ok_or_else(|| {
        SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{target_wc}' is not under version control"),
        )
    })?;
    let lock_dir = if node.kind == NodeKind::Dir {
        target_wc.clone()
    } else {
        target_wc.parent().unwrap_or_else(RelPath::root)
    };
    let anchor = anchor_of(&db, session)?;
    let target = wc::strip_rel(&target_wc, &anchor).ok_or_else(|| {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{target_wc}' is not below the session URL"),
        )
    })?;
    // The drive is anchored at the target's parent (svn's anchor/target split), named by its
    // path below the session URL: that parent must be where the session URL says it is. This
    // holds for a switch too: only the target moves, its parent still anchors the drive.
    if let Some(parent) = target_wc.parent().filter(|_| !target.is_root())
        && parent.starts_with(&anchor)
    {
        let session_rel = repos_relpath_of(session.repos_root(), session.session_url())?;
        let below = wc::strip_rel(&parent, &anchor).unwrap_or_else(RelPath::root);
        let expected = if below.is_root() {
            session_rel
        } else if session_rel.is_empty() {
            below.as_str().to_owned()
        } else {
            format!("{session_rel}/{below}")
        };
        let actual = db.read_base(&parent)?.and_then(|n| n.repos_relpath);
        if actual.as_deref() != Some(expected.as_str()) {
            return Err(SvnError::new(
                ErrorCode::WcInvalidOperation,
                format!(
                    "'{parent}' is switched to another location; update '{target_wc}' through a \
                     session at its parent's URL"
                ),
            ));
        }
    }

    db.wc_lock_obtain(&lock_dir, -1, ObtainMode::Cleanup)?;
    let _guard = LockGuard {
        db: Arc::clone(&db),
        dir: lock_dir,
    };
    // An earlier operation may have been interrupted: finish its queued work first.
    {
        let (db, cancel) = (Arc::clone(&db), cancel.clone());
        blocking(move || crate::wc::run_work_queue(&db, &cancel).map(|_| ())).await?;
    }

    // `svn update` first restores versioned items that were removed from disk behind its back.
    let restored = {
        let (db, cancel, tc) = (
            Arc::clone(&db),
            cancel.clone(),
            (target_wc.clone(), ctx.config.use_commit_times, params.depth),
        );
        blocking(move || {
            let restored = restore::queue_missing(&db, &tc.0, tc.2, tc.1)?;
            crate::wc::run_work_queue(&db, &cancel)?;
            Ok(restored)
        })
        .await?
    };
    for (path, kind) in &restored {
        ctx.notify(&UpdateEvent {
            path: path.clone(),
            kind: *kind,
            action: UpdateAction::Restored,
        });
    }

    let (switch_relpath, mode) = match &params.kind {
        UpdateKind::Update => (None, UpdateMode::Update),
        UpdateKind::Switch(url) => (
            Some(repos_relpath_of(session.repos_root(), url)?),
            UpdateMode::Switch(url.clone()),
        ),
    };
    let (root, uuid) = wc::repos_identity(&db)?;
    let setup = UpdateSetup {
        db: Arc::clone(&db),
        anchor: anchor.clone(),
        target: target.clone(),
        switch_relpath,
        depth: params.depth,
        sticky_depth: params.sticky_depth,
        allow_unversioned_obstructions: params.allow_unversioned_obstructions,
        repos_root: root.clone(),
        uuid,
        ctx: ctx.clone(),
        cancel: cancel.clone(),
        file_external: None,
    };
    let (editor, outcome) = UpdateEditor::new(setup)?;
    let report = build_report(
        Arc::clone(&db),
        anchor,
        target.clone(),
        params.depth,
        params.sticky_depth,
        root,
    );
    let req = UpdateRequest {
        revision: params.revision,
        target: target.clone(),
        // Like svn against a depth-aware server: only a sticky depth is sent; otherwise the
        // report carries the depths and the editor applies the requested one.
        depth: if params.sticky_depth {
            params.depth
        } else {
            Depth::Unknown
        },
        mode,
        send_copyfrom: false,
        ignore_ancestry: params.ignore_ancestry,
    };
    let result = session
        .do_update(req, report, Box::new(editor), cancel)
        .await;
    if let Err(e) = result {
        // Nodes committed before the failure have queued work; finish it so the tree matches.
        let db2 = Arc::clone(&db);
        let _ = blocking(move || crate::wc::run_work_queue(&db2, &CancelToken::new()).map(|_| ()))
            .await;
        return Err(e);
    }
    let reached = outcome
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .revision;
    if !params.sticky_depth
        && matches!(params.kind, UpdateKind::Update)
        && let Some(rev) = reached
    {
        // The update itself succeeded; markers it cannot record leave the working copy as it
        // was before this was implemented, so only cancellation is reported.
        if let Err(e) =
            record_unseen_children(session, &db, &target_wc, &target, params.depth, rev, cancel)
                .await
        {
            if e.code == ErrorCode::Cancelled {
                return Err(e);
            }
            tracing::warn!("could not record children outside the update depth: {e}");
        }
    }
    let out = outcome.lock().unwrap_or_else(PoisonError::into_inner);
    let mut events: Vec<UpdateEvent> = restored
        .into_iter()
        .map(|(path, kind)| UpdateEvent {
            path,
            kind,
            action: UpdateAction::Restored,
        })
        .collect();
    events.extend(out.events.iter().cloned());
    Ok(UpdateSummary {
        revision: out.revision,
        events,
        externals: Vec::new(),
    })
}

/// svn issue #3569: an update at a depth narrower than the recorded one does not hear about
/// additions outside that depth, so the directory would claim at the new revision to lack
/// children the repository has. Like svn's update editor (`fetch_dirents_func`), the target
/// directory (and, for `immediates`, its subdirectories) is listed at the reached revision and
/// every child the working copy does not know gets a `not-present` BASE row.
async fn record_unseen_children(
    session: &dyn RaSession,
    db: &Arc<WcDb>,
    target_wc: &RelPath,
    target: &RelPath,
    depth: Depth,
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<()> {
    if !matches!(depth, Depth::Empty | Depth::Files | Depth::Immediates) {
        return Ok(());
    }
    let rank = |d: Depth| match d {
        Depth::Empty => 0,
        Depth::Files => 1,
        Depth::Immediates => 2,
        _ => 3,
    };
    let dir_depth = |n: &svn_wc::db::Node| n.depth.unwrap_or(Depth::Infinity);
    let Some(node) = db.read_base(target_wc)? else {
        return Ok(());
    };
    if node.kind != NodeKind::Dir || node.presence != svn_wc::db::Presence::Normal {
        return Ok(());
    }
    // (working copy dir, session-relative dir, ambient depth)
    let mut dirs = Vec::new();
    if rank(dir_depth(&node)) > rank(depth) {
        dirs.push((target_wc.clone(), target.clone(), dir_depth(&node)));
    }
    if depth == Depth::Immediates {
        for child in db.children(target_wc)? {
            let Some(c) = db.read_base(&child)? else {
                continue;
            };
            if c.kind == NodeKind::Dir
                && c.presence == svn_wc::db::Presence::Normal
                && rank(dir_depth(&c)) > rank(Depth::Empty)
            {
                let name = RelPath::new(child.file_name().unwrap_or(""))?;
                dirs.push((child, wc::join_rel(target, &name)?, dir_depth(&c)));
            }
        }
    }
    for (wc_dir, session_dir, ambient) in dirs {
        let entries = session
            .list(&session_dir, Some(rev), Depth::Immediates, cancel)
            .await?;
        let Some(parent) = db.read_base(&wc_dir)? else {
            continue;
        };
        let parent_repos = parent.repos_relpath.clone().unwrap_or_default();
        for e in entries {
            let kind = if e.kind == NodeKind::Dir {
                NodeKind::Dir
            } else {
                NodeKind::File
            };
            if rank(ambient) < rank(Depth::Immediates) && kind == NodeKind::Dir {
                continue;
            }
            cancel.check()?;
            let name = RelPath::from_server(&e.name)?;
            if name.is_root() || name.as_str().contains('/') {
                continue;
            }
            let child = wc::join_rel(&wc_dir, &name)?;
            let repos_relpath = super::editor::repos_child(&parent_repos, name.as_str());
            let repos_id = parent.repos_id.unwrap_or_default();
            db.with_txn(|t| {
                if t.read_base(&child)?.is_some() {
                    return Ok(());
                }
                let (repos_root_url, repos_uuid) = t.repository(repos_id)?;
                t.base_add(
                    &child,
                    &svn_wc::db::BaseNode {
                        kind,
                        presence: svn_wc::db::Presence::NotPresent,
                        repos_root_url,
                        repos_uuid,
                        repos_relpath,
                        revision: rev.0,
                        changed_rev: None,
                        changed_date: None,
                        changed_author: None,
                        depth: None,
                        properties: None,
                        checksum: None,
                        symlink_target: None,
                        translated_size: None,
                        last_mod_time: None,
                        inherited_props: None,
                        dav_cache: None,
                    },
                )
            })?;
        }
    }
    Ok(())
}
