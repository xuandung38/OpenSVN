//! Orchestration of one update, switch or checkout drive.

use std::collections::HashSet;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use svn_ra_api::{RaSession, UpdateMode, UpdateRequest};
use svn_types::{
    CancelToken, Depth, ErrorCode, NodeKind, RelPath, RepoUrl, Result, Revnum, SvnError,
};
use svn_wc::db::{ObtainMode, WcDb};

use super::editor::UpdateEditor;
use super::reporter::build_report;
use super::restore;
use super::state::{CommittedNode, UpdateSetup};
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

/// Delays before asking again after a connection failure that left nothing done: a remote
/// status check (which writes nothing), or an update drive whose resumes commit nothing new to
/// the working copy. The length is how many such attempts in a row are made: a drive whose last
/// two resumes made no progress is not resumed again.
pub(crate) const RECONNECT_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// Most automatic resumes of one update drive. A resume that committed nodes no drive before it
/// had committed made progress, so the next resume waits only the first delay and does not
/// count against [`RECONNECT_DELAYS`]; this cap bounds a connection that keeps breaking anyway.
const MAX_RESUMES: usize = 20;

/// Whether a failed drive is resumed automatically: an update, checkout or switch that broke off
/// because the connection failed (see [`svn_types::ConnectionFailure`]). A switch is resumed
/// like an update: the next drive reports what the broken one committed (see
/// [`switch_resume`]).
fn can_reconnect(ctx: &ClientCtx, err: &SvnError) -> bool {
    err.code != ErrorCode::Cancelled
        && (err.connection_failure().is_some()
            || (ctx.network_wait.is_some() && err.is_unreachable()))
}

/// How a broken switch drive is driven again.
enum SwitchResume {
    /// The target was not moved yet: the same drive again.
    AsBefore,
    /// The target directory already points to the switch URL (the broken drive committed it,
    /// `incomplete`, and part of what is below it): the drive is anchored at the target
    /// itself, through a session at the switch URL, like svn's `svn_wc_get_actual_target2` for
    /// a switched directory. Its report then describes the moved directories with `set_path`
    /// and the ones still at their old location with `link_path`, so the server sends what
    /// each one lacks; the end of the drive moves the rest (see `bump_revisions_post_switch`).
    AtTarget(Box<dyn RaSession>),
    /// A file target that was already moved (the drive finished it, only the closing revision
    /// bump is missing), or a session that cannot be rooted at the switch URL: not driven
    /// again; the same switch finishes it.
    No,
}

/// Decides how a switch of `target_wc` to `url` (`relpath` below the repository root) is
/// resumed, from what the broken drive committed.
fn switch_resume(
    db: &WcDb,
    target_wc: &RelPath,
    url: &RepoUrl,
    relpath: &str,
    session: &dyn RaSession,
) -> Result<SwitchResume> {
    let Some(node) = db.read_base(target_wc)? else {
        return Ok(SwitchResume::AsBefore);
    };
    if node.repos_relpath.as_deref() != Some(relpath) {
        return Ok(SwitchResume::AsBefore);
    }
    if node.kind != NodeKind::Dir {
        return Ok(SwitchResume::No);
    }
    match session.reparented(url) {
        Ok(s) => Ok(SwitchResume::AtTarget(s)),
        Err(e) => {
            tracing::warn!(error = %e, "cannot resume the switch at its target");
            Ok(SwitchResume::No)
        }
    }
}

/// Network outages in a row, without progress in between, that a drive waits out before it
/// gives up: a server that drops off every time it is reached again is not waited for forever.
const MAX_OUTAGES: usize = 5;

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

/// The session's repository is the working copy's.
fn check_identity(db: &WcDb, session: &dyn RaSession) -> Result<()> {
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
    Ok(())
}

/// URL of `target_wc` (BASE `node`) when it is a directory switched away from its parent's
/// URL (its repository path is not its parent's plus its name); `None` otherwise.
fn switched_dir_url(
    db: &WcDb,
    target_wc: &RelPath,
    node: &svn_wc::db::Node,
    session: &dyn RaSession,
) -> Result<Option<RepoUrl>> {
    if node.kind != NodeKind::Dir {
        return Ok(None);
    }
    let (Some(parent), Some(name)) = (target_wc.parent(), target_wc.file_name()) else {
        return Ok(None);
    };
    let Some(parent_repos) = db.read_base(&parent)?.and_then(|n| n.repos_relpath) else {
        return Ok(None);
    };
    let repos = node.repos_relpath.clone().unwrap_or_default();
    let expected = if parent_repos.is_empty() {
        name.to_owned()
    } else {
        format!("{parent_repos}/{name}")
    };
    if repos == expected {
        return Ok(None);
    }
    Ok(Some(session.repos_root().join(&RelPath::new(&repos)?)?))
}

/// Working copy path of the directory the session URL points to.
pub fn anchor_of(db: &WcDb, session: &dyn RaSession) -> Result<RelPath> {
    check_identity(db, session)?;
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
    // A directory switched away from its parent's URL is its own anchor, reached through a
    // session at its own URL (svn's `svn_wc_get_actual_target2`): the report is then a plain
    // `set_path("")`. A `link_path` for it with entries below would make svn_repos read freed
    // memory (`reporter.c`, `delta_dirs`), and mod_dav_svn's child dies on an assertion.
    let reparented: Box<dyn RaSession>;
    let (session, anchor) = match switched_dir_url(&db, &target_wc, &node, session)? {
        Some(url) => {
            check_identity(&db, session)?;
            let session: &dyn RaSession = if session.session_url() == &url {
                session
            } else {
                reparented = session.reparented(&url)?;
                reparented.as_ref()
            };
            (session, target_wc.clone())
        }
        None => (session, anchor_of(&db, session)?),
    };
    let mut target = wc::strip_rel(&target_wc, &anchor).ok_or_else(|| {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{target_wc}' is not below the session URL"),
        )
    })?;
    // The drive is anchored at the target's parent (svn's anchor/target split), named by its
    // path below the session URL: that parent must be where the session URL says it is. This
    // holds for a switch too: only the target moves, its parent still anchors the drive. When
    // the parent (or a folder above it) is switched, the drive is anchored at the parent
    // itself, over a session at its own URL, as svn does.
    let reparented_to_parent: Box<dyn RaSession>;
    let mut session = session;
    let mut anchor = anchor;
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
        match actual {
            Some(actual) if actual == expected => {}
            Some(actual) => {
                let url = session.repos_root().join(&RelPath::new(&actual)?)?;
                reparented_to_parent = session.reparented(&url)?;
                session = reparented_to_parent.as_ref();
                target = RelPath::new(target_wc.file_name().unwrap_or(""))?;
                anchor = parent;
            }
            None => {
                return Err(SvnError::new(
                    ErrorCode::WcInvalidOperation,
                    format!("'{parent}' has no place in the repository; update its parent"),
                ));
            }
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

    // Decided before anything is restored, like svn (`is_empty_wc` runs before the crawl).
    let mut clean = Some(clean_target(&db, &target_wc, &params.kind)?);

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
    // A resumed switch whose target already moved is anchored at the target (see
    // [`SwitchResume::AtTarget`]).
    let mut at_target: Option<Box<dyn RaSession>> = None;
    let mut revision = params.revision;
    // Notifications of drives that broke off on a failed connection.
    let mut earlier_events: Vec<UpdateEvent> = Vec::new();
    let mut reconnects = 0usize;
    // Resumed drives in a row that committed nothing new.
    let mut stalled = 0usize;
    // Resumes after a network outage was waited out, and outages in a row without progress.
    let mut outage_resumes = 0usize;
    let mut outages = 0usize;
    // Every node state committed by the drives so far: the high-water mark of the progress. A
    // resumed drive sends the nodes of unfinished directories again (and notifies them again),
    // so only node states beyond this set are progress.
    let mut committed: HashSet<CommittedNode> = HashSet::new();
    let outcome = loop {
        // A resumed drive looks at the disk again: the broken one may have filled the target.
        let clean = match clean.take() {
            Some(c) => c,
            None => clean_target(&db, &target_wc, &params.kind)?,
        };
        let (drive_session, anchor, target) = match at_target.as_deref() {
            Some(s) => (s, target_wc.clone(), RelPath::root()),
            None => (session, anchor.clone(), target.clone()),
        };
        let setup = UpdateSetup {
            db: Arc::clone(&db),
            anchor: anchor.clone(),
            target: target.clone(),
            switch_relpath: switch_relpath.clone(),
            depth: params.depth,
            sticky_depth: params.sticky_depth,
            allow_unversioned_obstructions: params.allow_unversioned_obstructions,
            clean,
            repos_root: root.clone(),
            uuid: uuid.clone(),
            ctx: ctx.clone(),
            cancel: cancel.clone(),
            file_external: None,
        };
        let (editor, outcome) = UpdateEditor::new(setup)?;
        let report = build_report(
            Arc::clone(&db),
            anchor.clone(),
            target.clone(),
            params.depth,
            params.sticky_depth,
            root.clone(),
        );
        let req = UpdateRequest {
            revision,
            target: target.clone(),
            // Like svn against a depth-aware server: only a sticky depth is sent; otherwise the
            // report carries the depths and the editor applies the requested one.
            depth: if params.sticky_depth {
                params.depth
            } else {
                Depth::Unknown
            },
            mode: mode.clone(),
            send_copyfrom: false,
            ignore_ancestry: params.ignore_ancestry,
        };
        let result = drive_session
            .do_update(req, report, Box::new(editor), cancel)
            .await;
        let Err(e) = result else {
            break outcome;
        };
        if e.code == ErrorCode::Cancelled {
            // The user is waiting: what is still queued (the drive stopped committing when its
            // grace period ran out) is finished by the next update or cleanup, which run the
            // queue first.
            return Err(e);
        }
        // Nodes committed before the failure have queued work; finish it so the tree matches.
        let db2 = Arc::clone(&db);
        let _ = blocking(move || crate::wc::run_work_queue(&db2, &CancelToken::new()).map(|_| ()))
            .await;
        if !can_reconnect(ctx, &e) || reconnects - outage_resumes >= MAX_RESUMES {
            return Err(e);
        }
        if let (UpdateKind::Switch(url), Some(relpath)) = (&params.kind, switch_relpath.as_deref())
        {
            match switch_resume(&db, &target_wc, url, relpath, session)? {
                SwitchResume::AsBefore => {}
                SwitchResume::AtTarget(s) => at_target = Some(s),
                SwitchResume::No => return Err(e),
            }
        }
        // The working copy is consistent: directories the broken drive did not finish are
        // still `incomplete`, so the next report asks only for what is missing (the way an
        // interrupted checkout is resumed by `update`). It goes to the revision the broken
        // drive was reaching, not to a newer HEAD.
        let progressed = {
            let out = outcome.lock().unwrap_or_else(PoisonError::into_inner);
            revision = revision.or(out.revision);
            earlier_events.extend(out.events.iter().cloned());
            let mut new = false;
            for node in &out.committed {
                new |= committed.insert(node.clone());
            }
            new
        };
        if reconnects > 0 {
            stalled = if progressed { 0 } else { stalled + 1 };
        }
        if progressed {
            outages = 0;
        }
        // Is the server out of reach? Then wait for it rather than spend the resumes; once it
        // answers, resume at the same revision with a fresh budget.
        if let Some(wait) = ctx.network_wait.as_ref()
            && outages < MAX_OUTAGES
        {
            super::editor::failpoint!("update.before_network_wait");
            match crate::netwait::wait_for_network(session, wait, cancel).await? {
                crate::netwait::NetworkOutcome::Reachable => {}
                crate::netwait::NetworkOutcome::Restored => {
                    super::editor::failpoint!("update.after_network_restored");
                    outages += 1;
                    outage_resumes += 1;
                    reconnects += 1;
                    stalled = 0;
                    continue;
                }
                crate::netwait::NetworkOutcome::GaveUp => return Err(e),
            }
        }
        let Some(delay) = RECONNECT_DELAYS.get(stalled).copied() else {
            return Err(e);
        };
        reconnects += 1;
        tracing::info!(
            failure = ?e.connection_failure(),
            attempt = reconnects,
            "connection failed during the update drive; resuming on a fresh connection"
        );
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                cancel.check()?;
            }
            () = tokio::time::sleep(delay) => {}
        }
    };
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
    events.extend(earlier_events);
    events.extend(out.events.iter().cloned());
    if reconnects > 0 {
        // A resumed drive sends again the directories the broken one had opened: report each
        // notification once.
        let mut seen = std::collections::HashSet::new();
        events.retain(|e| seen.insert(format!("{e:?}")));
    }
    Ok(UpdateSummary {
        revision: out.revision,
        events,
        externals: Vec::new(),
    })
}

/// svn's "clean checkout" check (`is_empty_wc` in `libsvn_client/update.c`): an update (never a
/// switch) whose target is missing on disk, or is a directory without entries, adds what the
/// server sends without looking at wc.db (see [`UpdateSetup::clean`]). The administrative
/// directory only counts as empty when the target anchors svn's drive itself: the working copy
/// root or a switched directory. A file target, or a target that cannot be read, is not empty.
fn clean_target(db: &WcDb, target_wc: &RelPath, kind: &UpdateKind) -> Result<bool> {
    if !matches!(kind, UpdateKind::Update) {
        return Ok(false);
    }
    let Ok(path) = wc::abs_path(db, target_wc) else {
        return Ok(false);
    };
    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Ok(false),
    };
    let adm_is_empty = target_wc.is_root() || is_switched(db, target_wc)?;
    for entry in entries {
        let Ok(entry) = entry else {
            return Ok(false);
        };
        if !(adm_is_empty && entry.file_name() == ".svn") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `wc` is not where its parent's repository path puts it (svn's `svn_wc__db_is_switched`).
fn is_switched(db: &WcDb, wc: &RelPath) -> Result<bool> {
    let (Some(parent), Some(name)) = (wc.parent(), wc.file_name()) else {
        return Ok(false);
    };
    let own = db.read_base(wc)?.and_then(|n| n.repos_relpath);
    let parent = db.read_base(&parent)?.and_then(|n| n.repos_relpath);
    Ok(match (own, parent) {
        (Some(own), Some(parent)) => own != super::editor::repos_child(&parent, name),
        _ => false,
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
