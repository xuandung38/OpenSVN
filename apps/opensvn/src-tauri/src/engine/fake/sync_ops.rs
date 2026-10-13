//! Operations that talk to the fake repository: checkout, update, switch, commit, remote status.

use std::collections::BTreeMap;
use std::path::Path;

use bytes::Bytes;
use opensvn_ipc::{
    CheckoutRequest, CommitRequest, Depth, FileKind, FileStatus, RemoteStatus, Rev, UpdateRequest,
};
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{ErrorCode, NodeKind, PropChange, RelPath, Revnum, SvnError};

use super::FakeEngine;
use super::state::{FakeNode, FakeWc, Sched, rel};
use crate::engine::context::{OpContext, ProgressUpdate};
use crate::engine::facade::{RemoteScan, WcInfo, WcRef};

/// One node of a repository snapshot.
struct SnapItem {
    kind: FileKind,
    content: Bytes,
    props: BTreeMap<String, String>,
}

/// Repository tree below a path at one revision, keyed relative to that path.
struct Snapshot {
    rev: u64,
    items: BTreeMap<RelPath, SnapItem>,
}

fn depth_allows(depth: Depth, path: &RelPath, kind: FileKind) -> bool {
    let level = path.components().count();
    match depth {
        Depth::Infinity => true,
        Depth::Immediates => level <= 1,
        Depth::Files => level == 0 || (level == 1 && kind == FileKind::File),
        Depth::Empty => level == 0,
    }
}

fn session_path(repo_path: &str) -> Result<RelPath, SvnError> {
    RelPath::new(repo_path.trim_matches('/'))
}

fn lossy_props(props: &[PropChange]) -> BTreeMap<String, String> {
    props
        .iter()
        .filter_map(|p| {
            p.value
                .as_ref()
                .map(|v| (p.name.clone(), String::from_utf8_lossy(v).into_owned()))
        })
        .collect()
}

impl FakeEngine {
    async fn snapshot(
        &self,
        session: &FakeRaSession,
        base: &RelPath,
        rev: Option<Revnum>,
        ctx: &OpContext,
    ) -> Result<Snapshot, SvnError> {
        let rev = match rev {
            Some(r) => r,
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        if session.check_path(base, Some(rev), &ctx.cancel).await? != NodeKind::Dir {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("/{base} is not a directory at {rev}"),
            ));
        }
        let mut items = BTreeMap::new();
        items.insert(
            RelPath::root(),
            SnapItem {
                kind: FileKind::Dir,
                content: Bytes::new(),
                props: BTreeMap::new(),
            },
        );
        let entries = session
            .list(base, Some(rev), svn_types::Depth::Infinity, &ctx.cancel)
            .await?;
        for e in entries {
            let rp = RelPath::from_server(&e.name)?;
            if e.kind == NodeKind::Dir {
                items.insert(
                    rp,
                    SnapItem {
                        kind: FileKind::Dir,
                        content: Bytes::new(),
                        props: BTreeMap::new(),
                    },
                );
                continue;
            }
            let mut buf = Vec::new();
            let meta = session
                .get_file(&base_join(base, &rp)?, Some(rev), &mut buf, &ctx.cancel)
                .await?;
            items.insert(
                rp,
                SnapItem {
                    kind: FileKind::File,
                    content: buf.into(),
                    props: lossy_props(&meta.props),
                },
            );
        }
        Ok(Snapshot { rev: rev.0, items })
    }

    pub(super) async fn do_checkout(
        &self,
        req: &CheckoutRequest,
        ctx: &OpContext,
    ) -> Result<WcInfo, SvnError> {
        let root = Path::new(&req.path);
        if !root.is_absolute() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "checkout path must be absolute",
            ));
        }
        let (session, base) = self.locate(&req.url)?;
        if self.with_wc(root, |_| Ok(())).is_ok() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("{} is already a working copy", root.display()),
            ));
        }
        session.ensure_authenticated(&ctx.cancel).await?;
        let snap = self
            .snapshot(&session, &base, req.rev.map(|r| Revnum(r.0)), ctx)
            .await?;
        let total = snap.items.len() as u64;
        // Network phase: cancelling aborts at once and leaves nothing behind.
        for (i, (path, _)) in snap.items.iter().enumerate() {
            self.network_tick(ctx).await?;
            ctx.progress.report(ProgressUpdate {
                phase: "network".into(),
                current: Some(i as u64 + 1),
                total: Some(total),
                current_path: Some(path.as_str().to_owned()),
                ..ProgressUpdate::default()
            });
        }
        let mut wc = FakeWc {
            root: root.to_path_buf(),
            repo_root_url: session.repos_root().to_string(),
            uuid: session.uuid().to_owned(),
            path_in_repo: if base.is_root() {
                String::new()
            } else {
                format!("/{base}")
            },
            format: req.format,
            store_pristine: req.store_pristine,
            nodes: BTreeMap::new(),
            ignores: Vec::new(),
            dead_owner_locks: Vec::new(),
            foreign_locks: Vec::new(),
        };
        for (i, (path, item)) in snap.items.into_iter().enumerate() {
            // Work-queue phase: a cancel here discards the partial working copy.
            self.queue_tick(ctx).await?;
            if depth_allows(req.depth, &path, item.kind) {
                let mut node = match item.kind {
                    FileKind::Dir => FakeNode::dir(snap.rev),
                    _ => FakeNode::file(&item.content, snap.rev),
                };
                node.props = item.props.clone();
                node.base_props = item.props;
                wc.nodes.insert(path.clone(), node);
            }
            ctx.progress.report(ProgressUpdate {
                phase: "work-queue".into(),
                current: Some(i as u64 + 1),
                total: Some(total),
                current_path: Some(path.as_str().to_owned()),
                ..ProgressUpdate::default()
            });
        }
        let info = wc.info();
        self.wcs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.to_path_buf(), wc);
        Ok(info)
    }

    /// Applies `snap` to the working copy item by item. `scope` limits the update to subtrees.
    async fn apply_snapshot(
        &self,
        wc: &WcRef,
        snap: Snapshot,
        scope: &[RelPath],
        ctx: &OpContext,
    ) -> Result<u64, SvnError> {
        let in_scope = |p: &RelPath| scope.is_empty() || scope.iter().any(|s| p.starts_with(s));
        let total = snap.items.len() as u64;
        let mut seen = Vec::new();
        for (i, (path, item)) in snap.items.iter().enumerate() {
            if !in_scope(path) {
                continue;
            }
            self.queue_tick(ctx).await?;
            self.with_wc(&wc.root, |w| {
                apply_item(w, path, item, snap.rev);
                Ok(())
            })?;
            seen.push(path.clone());
            ctx.progress.report(ProgressUpdate {
                phase: "work-queue".into(),
                current: Some(i as u64 + 1),
                total: Some(total),
                current_path: Some(path.as_str().to_owned()),
                ..ProgressUpdate::default()
            });
        }
        self.with_wc(&wc.root, |w| {
            let gone: Vec<RelPath> = w
                .nodes
                .iter()
                .filter(|(p, n)| n.is_versioned() && in_scope(p) && !snap.items.contains_key(*p))
                .map(|(p, _)| p.clone())
                .collect();
            for p in gone {
                let Some(n) = w.nodes.get_mut(&p) else {
                    continue;
                };
                if n.sched == Sched::Normal && n.work == n.base && !n.conflicted {
                    w.nodes.remove(&p);
                } else {
                    n.tree_conflict = true;
                }
            }
            Ok(())
        })?;
        Ok(snap.rev)
    }

    pub(super) async fn do_update(
        &self,
        wc: &WcRef,
        req: &UpdateRequest,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        let info = self.with_wc(&wc.root, |w| Ok(w.info()))?;
        let session = self.repo_by_root(&info.repo_root_url)?;
        let base = session_path(&info.path_in_repo)?;
        let scope = req
            .paths
            .iter()
            .flatten()
            .map(|p| rel(p))
            .collect::<Result<Vec<_>, _>>()?;
        session.ensure_authenticated(&ctx.cancel).await?;
        let snap = self
            .snapshot(&session, &base, req.rev.map(|r| Revnum(r.0)), ctx)
            .await?;
        for (i, (path, _)) in snap.items.iter().enumerate() {
            self.network_tick(ctx).await?;
            ctx.progress.report(ProgressUpdate {
                phase: "network".into(),
                current: Some(i as u64 + 1),
                total: Some(snap.items.len() as u64),
                current_path: Some(path.as_str().to_owned()),
                ..ProgressUpdate::default()
            });
        }
        self.apply_snapshot(wc, snap, &scope, ctx).await.map(Rev)
    }

    pub(super) async fn do_switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        let info = self.with_wc(&wc.root, |w| Ok(w.info()))?;
        let (session, base) = self.locate(url)?;
        if session.uuid() != info.repo_uuid {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "cannot switch to a different repository",
            ));
        }
        session.ensure_authenticated(&ctx.cancel).await?;
        let snap = self
            .snapshot(&session, &base, rev.map(|r| Revnum(r.0)), ctx)
            .await?;
        for _ in 0..snap.items.len() {
            self.network_tick(ctx).await?;
        }
        let new_path = if base.is_root() {
            String::new()
        } else {
            format!("/{base}")
        };
        self.with_wc(&wc.root, |w| {
            w.path_in_repo = new_path;
            Ok(())
        })?;
        self.apply_snapshot(wc, snap, &[], ctx).await.map(Rev)
    }

    pub(super) async fn do_commit(
        &self,
        wc: &WcRef,
        req: &CommitRequest,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        let info = self.with_wc(&wc.root, |w| Ok(w.info()))?;
        let session = self.repo_by_root(&info.repo_root_url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        let plan = self.with_wc(&wc.root, |w| plan_commit(w, &req.paths))?;
        if plan.changes.is_empty() {
            return Err(SvnError::new(ErrorCode::Internal, "nothing to commit"));
        }
        for (path, base_rev) in &plan.check_up_to_date {
            let rp = session_path(&plan.repo_path(path))?;
            let entry = session.stat(&rp, None, &ctx.cancel).await?;
            if entry
                .and_then(|e| e.created_rev)
                .is_some_and(|r| r.0 > *base_rev)
            {
                return Err(SvnError::new(
                    ErrorCode::OutOfDate,
                    format!("{path} is out of date; update first"),
                )
                .with_svn_code(160024));
            }
        }
        for (i, c) in plan.paths.iter().enumerate() {
            self.network_tick(ctx).await?;
            ctx.progress.report(ProgressUpdate {
                phase: "network".into(),
                current: Some(i as u64 + 1),
                total: Some(plan.paths.len() as u64),
                current_path: Some(c.as_str().to_owned()),
                ..ProgressUpdate::default()
            });
        }
        // Past this point the commit is final: the revision exists on the "server".
        ctx.check()?;
        let new_rev = session.seed("tester", &req.message, &plan.changes_with_prefix()?)?;
        if !req.keep_locks {
            for (path, token) in &plan.locked {
                let rp = session_path(&plan.repo_path(path))?;
                session.unlock(&rp, Some(token), false, &ctx.cancel).await?;
            }
        }
        self.with_wc(&wc.root, |w| {
            finish_commit(w, &plan, new_rev.0, req.keep_locks);
            Ok(())
        })?;
        Ok(Rev(new_rev.0))
    }

    pub(super) async fn do_status_remote(
        &self,
        wc: &WcRef,
        ctx: &OpContext,
    ) -> Result<RemoteScan, SvnError> {
        let info = self.with_wc(&wc.root, |w| Ok(w.info()))?;
        let session = self.repo_by_root(&info.repo_root_url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        self.network_tick(ctx).await?;
        let head = session.youngest_rev(&ctx.cancel).await?;
        let base = session_path(&info.path_in_repo)?;
        let locks = session
            .get_locks(&base, svn_types::Depth::Infinity, &ctx.cancel)
            .await?;
        let nodes: Vec<(RelPath, u64)> = self.with_wc(&wc.root, |w| {
            Ok(w.nodes
                .iter()
                .filter(|(p, n)| n.sched == Sched::Normal && !p.is_root())
                .map(|(p, n)| (p.clone(), n.rev))
                .collect())
        })?;
        let mut remote = Vec::new();
        for (path, node_rev) in nodes {
            let repo_rel = base_join(&base, &path)?;
            let entry = session.stat(&repo_rel, None, &ctx.cancel).await?;
            let remote_rev = entry
                .as_ref()
                .and_then(|e| e.created_rev)
                .map_or(head.0, |r| r.0);
            let out_of_date = entry.is_none() || remote_rev > node_rev;
            let remote_lock = locks
                .iter()
                .find(|l| l.path.trim_start_matches('/') == repo_rel.as_str())
                .map(|l| opensvn_ipc::LockInfo {
                    owner: l.owner.clone(),
                    comment: l.comment.clone(),
                    created_at: opensvn_ipc::time_fmt::timestamp_to_iso(l.created),
                    token: None,
                    mine: false,
                });
            if out_of_date || remote_lock.is_some() {
                remote.push((path, out_of_date, remote_rev, remote_lock));
            }
        }
        let statuses = self.with_wc(&wc.root, |w| {
            Ok(remote
                .into_iter()
                .filter_map(|(path, out_of_date, rev, lock)| {
                    let mut s: FileStatus = w.status_forced(&path)?;
                    s.remote = Some(RemoteStatus {
                        out_of_date,
                        remote_rev: Some(Rev(rev)),
                        remote_lock: lock,
                    });
                    Some(s)
                })
                .collect::<Vec<_>>())
        })?;
        let incoming =
            count_revisions(&session, &base, info.base_rev.0, head.0, &ctx.cancel).await?;
        Ok(RemoteScan {
            statuses,
            incoming,
            latest_rev: Rev(head.0),
        })
    }
}

/// Number of revisions in `(after, upto]` that touch `path`.
async fn count_revisions(
    session: &FakeRaSession,
    path: &RelPath,
    after: u64,
    upto: u64,
    cancel: &svn_types::CancelToken,
) -> Result<u32, SvnError> {
    if upto <= after {
        return Ok(0);
    }
    let req = svn_ra_api::LogRequest {
        paths: vec![path.clone()],
        start: Some(Revnum(upto)),
        end: Some(Revnum(after + 1)),
        limit: None,
        discover_changed_paths: false,
        strict_node_history: false,
    };
    let mut n = 0u32;
    session
        .log(
            &req,
            &mut |_| {
                n += 1;
                Ok(())
            },
            cancel,
        )
        .await?;
    Ok(n)
}

fn base_join(base: &RelPath, rel: &RelPath) -> Result<RelPath, SvnError> {
    match (base.is_root(), rel.is_root()) {
        (_, true) => Ok(base.clone()),
        (true, false) => Ok(rel.clone()),
        (false, false) => RelPath::new(&format!("{base}/{rel}")),
    }
}

/// Folds one repository node into the working copy (the "work queue item").
fn apply_item(w: &mut FakeWc, path: &RelPath, item: &SnapItem, rev: u64) {
    match w.nodes.get_mut(path) {
        None => {
            let mut node = match item.kind {
                FileKind::Dir => FakeNode::dir(rev),
                _ => FakeNode::file(&item.content, rev),
            };
            node.props = item.props.clone();
            node.base_props = item.props.clone();
            w.nodes.insert(path.clone(), node);
        }
        Some(n) => match (n.sched, item.kind) {
            (Sched::Unversioned, _) | (Sched::Added, _) => {
                // The incoming node collides with a local one.
                n.tree_conflict = true;
            }
            (Sched::Deleted, _) => {
                if n.base.as_deref() != Some(&item.content[..]) {
                    n.tree_conflict = true;
                }
                n.rev = rev;
            }
            (Sched::Normal, FileKind::Dir | FileKind::Symlink) => n.rev = rev,
            (Sched::Normal, FileKind::File) => {
                let changed_upstream = n.base.as_deref() != Some(&item.content[..]);
                if changed_upstream {
                    let locally_modified = n.work != n.base;
                    n.base = Some(item.content.clone());
                    if locally_modified {
                        n.conflicted = true;
                    } else {
                        n.work = Some(item.content.clone());
                    }
                }
                n.base_props = item.props.clone();
                n.rev = rev;
            }
        },
    }
}

/// What a commit will send and what to do with the working copy afterwards.
struct CommitPlan {
    changes: Vec<FakeChange>,
    /// Working copy paths that take part, in commit order.
    paths: Vec<RelPath>,
    /// Roots of deleted subtrees (removed from the working copy afterwards).
    deleted_roots: Vec<RelPath>,
    /// Modified files and the revision they are based on (out-of-date check).
    check_up_to_date: Vec<(RelPath, u64)>,
    /// Locked files and their tokens.
    locked: Vec<(RelPath, String)>,
    path_in_repo: String,
}

impl CommitPlan {
    fn repo_path(&self, path: &RelPath) -> String {
        format!("{}/{}", self.path_in_repo, path)
    }

    /// Changes with repository-relative paths (the plan stores working copy relative ones).
    fn changes_with_prefix(&self) -> Result<Vec<FakeChange>, SvnError> {
        let prefix = self.path_in_repo.trim_matches('/');
        let map = |p: &RelPath| -> Result<RelPath, SvnError> {
            if prefix.is_empty() {
                Ok(p.clone())
            } else {
                RelPath::new(&format!("{prefix}/{p}"))
            }
        };
        self.changes
            .iter()
            .map(|c| {
                Ok(match c {
                    FakeChange::PutFile(p, b) => FakeChange::PutFile(map(p)?, b.clone()),
                    FakeChange::PutDir(p) => FakeChange::PutDir(map(p)?),
                    FakeChange::SetProp(p, c) => FakeChange::SetProp(map(p)?, c.clone()),
                    FakeChange::Delete(p) => FakeChange::Delete(map(p)?),
                })
            })
            .collect()
    }
}

fn plan_commit(w: &FakeWc, selection: &[String]) -> Result<CommitPlan, SvnError> {
    let mut plan = CommitPlan {
        changes: Vec::new(),
        paths: Vec::new(),
        deleted_roots: Vec::new(),
        check_up_to_date: Vec::new(),
        locked: Vec::new(),
        path_in_repo: w.path_in_repo.clone(),
    };
    for p in w.expand_selection(selection)? {
        let n = w.node(&p)?;
        if p.is_root() {
            continue;
        }
        if n.conflicted || n.tree_conflict {
            return Err(SvnError::new(
                ErrorCode::ConflictBlocksCommit,
                format!("{p} has unresolved conflicts"),
            ));
        }
        let before = plan.changes.len();
        let missing = n.kind == FileKind::File && n.work.is_none();
        // A versioned file that vanished from disk is removed automatically.
        let remove = n.sched == Sched::Deleted || (n.sched == Sched::Normal && missing);
        if remove {
            if !plan.deleted_roots.iter().any(|d| p.starts_with(d)) {
                plan.changes.push(FakeChange::Delete(p.clone()));
                plan.deleted_roots.push(p.clone());
            }
        } else {
            match n.sched {
                Sched::Added | Sched::Unversioned => {
                    if missing {
                        return Err(SvnError::new(
                            ErrorCode::PathNotFound,
                            format!("{p} is scheduled for addition but missing"),
                        ));
                    }
                    plan.changes.push(match (n.kind, &n.work) {
                        (FileKind::File, Some(b)) => FakeChange::PutFile(p.clone(), b.clone()),
                        _ => FakeChange::PutDir(p.clone()),
                    });
                }
                Sched::Normal | Sched::Deleted => {
                    if let (FileKind::File, Some(b)) = (n.kind, &n.work)
                        && n.work != n.base
                    {
                        plan.changes.push(FakeChange::PutFile(p.clone(), b.clone()));
                        plan.check_up_to_date.push((p.clone(), n.rev));
                    }
                }
            }
        }
        if n.sched != Sched::Deleted && !missing && n.props != n.base_props {
            for (k, v) in n
                .props
                .iter()
                .filter(|(k, v)| n.base_props.get(*k) != Some(*v))
            {
                plan.changes.push(FakeChange::SetProp(
                    p.clone(),
                    PropChange {
                        name: k.clone(),
                        value: Some(Bytes::from(v.clone())),
                    },
                ));
            }
            for k in n.base_props.keys().filter(|k| !n.props.contains_key(*k)) {
                plan.changes.push(FakeChange::SetProp(
                    p.clone(),
                    PropChange {
                        name: k.clone(),
                        value: None,
                    },
                ));
            }
        }
        if plan.changes.len() > before {
            if let Some(token) = n.lock.as_ref().and_then(|l| l.token.clone()) {
                plan.locked.push((p.clone(), token));
            }
            plan.paths.push(p);
        }
    }
    Ok(plan)
}

fn finish_commit(w: &mut FakeWc, plan: &CommitPlan, rev: u64, keep_locks: bool) {
    w.nodes
        .retain(|p, _| !plan.deleted_roots.iter().any(|d| p.starts_with(d)));
    for p in &plan.paths {
        let Some(n) = w.nodes.get_mut(p) else {
            continue;
        };
        n.sched = Sched::Normal;
        n.base = n.work.clone();
        n.base_props = n.props.clone();
        n.rev = rev;
        n.moved_from = None;
        n.moved_to = None;
        n.changelist = None;
        if !keep_locks {
            n.lock = None;
        }
    }
}
