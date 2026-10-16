//! Repository reads and server-side edits of the fake engine.

use std::path::Path;

use bytes::Bytes;
use opensvn_ipc::time_fmt::timestamp_to_iso;
use opensvn_ipc::{
    BlameLine, BranchKind, BranchNode, CopyLocationRequest, EntryKind, FileDiff, Layout,
    LogGetRequest, LogPage, RepoEntry, RepoInfo, RepoListing, Rev, Size, TextSide,
};
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{LogRequest, RaSession};
use svn_types::{ErrorCode, NodeKind, RelPath, Revnum, SvnError};

use super::FakeEngine;
use super::state::{Sched, rel};
use crate::engine::context::{OpContext, ProgressUpdate};
use crate::engine::facade::{DiffOptions, EditOp, LockScope, LogScope, RemoteEditPlan, WcRef};
use crate::engine::mapping::{self, entry_kind, revnum};
use crate::engine::text::{file_diff, match_lines, text_side};

/// Largest file `repo_cat` returns inline.
const CAT_LIMIT: usize = 5 * 1024 * 1024;
/// Largest local file `remote_edit` uploads.
const UPLOAD_LIMIT: u64 = 64 * 1024 * 1024;

fn internal(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Internal, msg)
}

/// Property rows of a working copy diff (`before` = pristine, `after` = working), by name.
fn prop_rows(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Vec<opensvn_ipc::PropChange> {
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|n| before.get(*n) != after.get(*n))
        .map(|n| opensvn_ipc::PropChange {
            name: n.clone(),
            before: before.get(n).cloned(),
            after: after.get(n).cloned(),
        })
        .collect()
}

impl FakeEngine {
    async fn read_file(
        &self,
        session: &FakeRaSession,
        path: &RelPath,
        rev: Option<Revnum>,
        ctx: &OpContext,
    ) -> Result<(u64, Vec<u8>), SvnError> {
        let mut buf = Vec::new();
        let meta = session.get_file(path, rev, &mut buf, &ctx.cancel).await?;
        Ok((meta.rev.0, buf))
    }

    pub(super) async fn do_repo_info(
        &self,
        url: &str,
        peg: Option<Rev>,
        ctx: &OpContext,
    ) -> Result<RepoInfo, SvnError> {
        let (session, path) = self.locate(url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        let youngest = session.youngest_rev(&ctx.cancel).await?;
        let kind = match session.check_path(&path, revnum(peg), &ctx.cancel).await? {
            NodeKind::Dir => EntryKind::Dir,
            NodeKind::File => EntryKind::File,
            _ => {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("{url} does not exist"),
                ));
            }
        };
        Ok(RepoInfo {
            repo_root_url: session.repos_root().to_string(),
            repo_uuid: session.uuid().to_owned(),
            youngest_rev: Rev(youngest.0),
            kind,
        })
    }

    pub(super) async fn do_repo_list(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> Result<RepoListing, SvnError> {
        let (session, path) = self.locate(url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        let listing = session
            .list_counted(
                &path,
                revnum(rev),
                svn_types::Depth::Immediates,
                &ctx.cancel,
            )
            .await?;
        let mut out = Vec::with_capacity(listing.entries.len());
        for e in listing.entries {
            let created = e.created_rev.unwrap_or(Revnum(0));
            let (author, date) = self.rev_author_date(&session, created, ctx).await?;
            out.push(RepoEntry {
                name: e.name,
                kind: entry_kind(e.kind),
                size: e.size.map(Size),
                created_rev: Rev(created.0),
                last_author: author,
                date,
                lock: e.lock.map(mapping::lock),
            });
        }
        out.sort_by(|a, b| {
            (a.kind == EntryKind::File, a.name.to_lowercase())
                .cmp(&(b.kind == EntryKind::File, b.name.to_lowercase()))
        });
        Ok(RepoListing {
            entries: out,
            hidden: u32::try_from(listing.rejected).unwrap_or(u32::MAX),
        })
    }

    async fn rev_author_date(
        &self,
        session: &FakeRaSession,
        rev: Revnum,
        ctx: &OpContext,
    ) -> Result<(Option<String>, String), SvnError> {
        let req = LogRequest {
            paths: Vec::new(),
            start: Some(rev),
            end: Some(rev),
            limit: Some(1),
            discover_changed_paths: false,
            strict_node_history: false,
        };
        let mut found = (None, "1970-01-01T00:00:00Z".to_owned());
        session
            .log(
                &req,
                &mut |e| {
                    found = (
                        e.author,
                        e.date.map_or_else(|| found.1.clone(), timestamp_to_iso),
                    );
                    Ok(())
                },
                &ctx.cancel,
            )
            .await?;
        Ok(found)
    }

    pub(super) async fn do_repo_cat(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> Result<TextSide, SvnError> {
        let (session, path) = self.locate(url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        let (rev, data) = self.read_file(&session, &path, revnum(rev), ctx).await?;
        if data.len() > CAT_LIMIT {
            return Err(internal(
                "file is too large to show; save it to disk instead",
            ));
        }
        Ok(text_side(
            &data,
            format!("{}@r{rev}", path.file_name().unwrap_or("")),
        ))
    }

    pub(super) async fn do_repo_save(
        &self,
        url: &str,
        rev: Option<Rev>,
        save_to: &Path,
        ctx: &OpContext,
    ) -> Result<(), SvnError> {
        if !save_to.is_absolute() {
            return Err(internal("save path must be absolute"));
        }
        let (session, path) = self.locate(url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        self.network_tick(ctx).await?;
        let (_, data) = self.read_file(&session, &path, revnum(rev), ctx).await?;
        ctx.progress.report(ProgressUpdate {
            phase: "network".into(),
            bytes_done: Some(data.len() as u64),
            bytes_total: Some(data.len() as u64),
            ..ProgressUpdate::default()
        });
        // The cancel point is before the write: a file is either complete or absent.
        ctx.check()?;
        tokio::fs::write(save_to, &data)
            .await
            .map_err(|e| SvnError::from_io(format!("write {}", save_to.display()), &e))
    }

    pub(super) async fn do_diff_wc(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> Result<FileDiff, SvnError> {
        let rp = rel(path)?;
        let (node, repo_root, repo_path) = self.with_wc(&wc.root, |w| {
            Ok((
                w.node(&rp)?.clone(),
                w.repo_root_url.clone(),
                w.repo_path(&rp),
            ))
        })?;
        // Pending deletions have no working properties (like the real engine).
        let working_props = if node.sched == Sched::Deleted {
            Default::default()
        } else {
            node.props.clone()
        };
        if node.kind == opensvn_ipc::FileKind::Dir {
            if against.is_some() {
                return Err(SvnError::new(
                    ErrorCode::WcInvalidOperation,
                    format!("'{rp}' is a directory; only files can be compared with a revision"),
                ));
            }
            // A folder has no text: only its property changes (svn:ignore, ...).
            return Ok(FileDiff {
                path: path.to_owned(),
                binary: false,
                mime: None,
                left: None,
                right: None,
                hunks: None,
                prop_changes: prop_rows(&node.base_props, &working_props),
                image: None,
                too_large: None,
                too_complex: None,
                office: None,
            });
        }
        let (left, left_props) = match against {
            None => (
                node.base
                    .as_ref()
                    .map(|b| (b.to_vec(), format!("{rp} (base)"))),
                node.base_props.clone(),
            ),
            Some(r) => {
                let session = self.repo_by_root(&repo_root)?;
                let path = RelPath::new(repo_path.trim_matches('/'))?;
                let mut data = Vec::new();
                let meta = session
                    .get_file(&path, Some(Revnum(r.0)), &mut data, &ctx.cancel)
                    .await?;
                // Like the real engine: the properties of the file at that revision against the
                // working ones (entry and working copy properties are not user properties).
                let props = meta
                    .props
                    .iter()
                    .filter(|p| !p.name.starts_with("svn:entry:") && !p.name.starts_with("svn:wc:"))
                    .filter_map(|p| {
                        let v = p.value.as_ref()?;
                        Some((p.name.clone(), String::from_utf8_lossy(v).into_owned()))
                    })
                    .collect();
                (Some((data, format!("{rp}@r{}", r.0))), props)
            }
        };
        let right = node
            .work
            .as_ref()
            .map(|b| (b.to_vec(), format!("{rp} (working)")));
        let mut diff = file_diff(path, left, right, opts, ctx)?;
        diff.prop_changes = prop_rows(&left_props, &working_props);
        Ok(diff)
    }

    pub(super) async fn do_diff_rev(
        &self,
        url: &str,
        from: Option<Rev>,
        to: Rev,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> Result<FileDiff, SvnError> {
        let (session, path) = self.locate(url)?;
        let from = from.map_or(to.0.saturating_sub(1), |r| r.0);
        let name = path.file_name().unwrap_or("").to_owned();
        let read = |rev: u64| {
            let (session, path) = (session.clone(), path.clone());
            async move {
                if rev == 0 {
                    return Ok(None);
                }
                match self
                    .read_file(&session, &path, Some(Revnum(rev)), ctx)
                    .await
                {
                    Ok((_, d)) => Ok(Some(d)),
                    Err(e) if e.code == ErrorCode::PathNotFound => Ok(None),
                    Err(e) => Err(e),
                }
            }
        };
        let left = read(from).await?.map(|d| (d, format!("{name}@r{from}")));
        let right = read(to.0).await?.map(|d| (d, format!("{name}@r{}", to.0)));
        file_diff(&name, left, right, opts, ctx)
    }

    pub(super) async fn do_log(
        &self,
        scope: &LogScope,
        req: &LogGetRequest,
        ctx: &OpContext,
    ) -> Result<LogPage, SvnError> {
        let (session, path) = match scope {
            LogScope::Url(url) => self.locate(url)?,
            LogScope::Repo(loc) => (
                self.repo_by_root(&loc.repo_root)?,
                RelPath::new(loc.repos_path.trim_matches('/'))?,
            ),
        };
        session.ensure_authenticated(&ctx.cancel).await?;
        self.log_wait().await;
        let filter = req
            .filter
            .as_ref()
            .map(|f| f.to_lowercase())
            .filter(|f| !f.is_empty());
        let limit = req.limit.clamp(1, 1000) as usize;
        let ra = LogRequest {
            paths: vec![path],
            start: revnum(req.from_rev),
            end: revnum(req.to_rev),
            limit: None,
            discover_changed_paths: req.with_paths,
            strict_node_history: req.stop_on_copy.unwrap_or(false),
        };
        let mut matched = Vec::new();
        session
            .log(
                &ra,
                &mut |e| {
                    let hit = filter.as_ref().is_none_or(|f| {
                        e.message
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(f)
                            || e.author.as_deref().unwrap_or("").to_lowercase().contains(f)
                    });
                    if hit && matched.len() <= limit {
                        matched.push(e);
                    }
                    Ok(())
                },
                &ctx.cancel,
            )
            .await?;
        let next_rev = (matched.len() > limit).then(|| Rev(matched[limit].rev.0));
        matched.truncate(limit);
        Ok(LogPage {
            entries: matched.into_iter().map(mapping::log_entry).collect(),
            next_rev,
        })
    }

    pub(super) async fn do_blame(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> Result<Vec<BlameLine>, SvnError> {
        let (session, path) = self.locate(url)?;
        session.ensure_authenticated(&ctx.cancel).await?;
        let head = match rev {
            Some(r) => Revnum(r.0),
            None => session.youngest_rev(&ctx.cancel).await?,
        };
        let ra = LogRequest {
            paths: vec![path.clone()],
            start: Some(head),
            end: None,
            limit: None,
            discover_changed_paths: false,
            strict_node_history: true,
        };
        let mut revs = Vec::new();
        session
            .log(
                &ra,
                &mut |e| {
                    revs.push(e);
                    Ok(())
                },
                &ctx.cancel,
            )
            .await?;
        revs.reverse(); // oldest first
        let mut lines: Vec<(Rev, Option<String>, String)> = Vec::new();
        let mut previous: Vec<String> = Vec::new();
        for entry in revs {
            self.network_tick(ctx).await?;
            let (_, data) = match self.read_file(&session, &path, Some(entry.rev), ctx).await {
                Ok(v) => v,
                Err(e) if e.code == ErrorCode::PathNotFound => continue,
                Err(e) => return Err(e),
            };
            let text = String::from_utf8_lossy(&data).into_owned();
            let current: Vec<String> = text.lines().map(str::to_owned).collect();
            let old: Vec<&str> = previous.iter().map(String::as_str).collect();
            let new: Vec<&str> = current.iter().map(String::as_str).collect();
            let matches = match_lines(&old, &new);
            let date = entry.date.map_or_else(String::new, timestamp_to_iso);
            let carried = std::mem::take(&mut lines);
            lines = current
                .iter()
                .zip(matches)
                .map(|(_, m)| match m.and_then(|i| carried.get(i)) {
                    Some((r, a, d)) => (*r, a.clone(), d.clone()),
                    None => (Rev(entry.rev.0), entry.author.clone(), date.clone()),
                })
                .collect();
            previous = current;
        }
        Ok(lines
            .into_iter()
            .zip(previous)
            .map(|((r, author, date), text)| BlameLine {
                rev: r,
                author,
                date,
                text,
            })
            .collect())
    }

    pub(super) async fn do_lock(
        &self,
        scope: &LockScope,
        comment: Option<&str>,
        steal: bool,
        ctx: &OpContext,
    ) -> Result<(), SvnError> {
        for (session, repo_path, wc_target) in self.lock_targets(scope)? {
            self.network_tick(ctx).await?;
            session.ensure_authenticated(&ctx.cancel).await?;
            let base_rev = wc_target.as_ref().map(|(_, _, rev)| Revnum(*rev));
            let lock = session
                .lock(&repo_path, comment, steal, base_rev, &ctx.cancel)
                .await?;
            if let Some((root, wc_rel, _)) = wc_target {
                self.with_wc(&root, |w| {
                    w.node_mut(&wc_rel)?.lock = Some(opensvn_ipc::LockInfo {
                        owner: lock.owner.clone(),
                        comment: lock.comment.clone(),
                        created_at: timestamp_to_iso(lock.created),
                        token: Some(lock.token.clone()),
                        mine: true,
                    });
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    pub(super) async fn do_unlock(
        &self,
        scope: &LockScope,
        break_lock: bool,
        ctx: &OpContext,
    ) -> Result<(), SvnError> {
        for (session, repo_path, wc_target) in self.lock_targets(scope)? {
            self.network_tick(ctx).await?;
            session.ensure_authenticated(&ctx.cancel).await?;
            // A break presents no token, like the real engine (`svn unlock --force`).
            let token = match &wc_target {
                Some((root, wc_rel, _)) if !break_lock => self.with_wc(root, |w| {
                    Ok(w.node(wc_rel)?.lock.as_ref().and_then(|l| l.token.clone()))
                })?,
                _ => None,
            };
            session
                .unlock(&repo_path, token.as_deref(), break_lock, &ctx.cancel)
                .await?;
            if let Some((root, wc_rel, _)) = wc_target {
                self.with_wc(&root, |w| {
                    w.node_mut(&wc_rel)?.lock = None;
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    pub(super) async fn do_lock_conflicts(
        &self,
        scope: &LockScope,
        ctx: &OpContext,
    ) -> Result<Vec<opensvn_ipc::LockConflict>, SvnError> {
        let requested: Vec<String> = match scope {
            LockScope::Urls(urls) => urls.clone(),
            LockScope::Wc { paths, .. } => paths.clone(),
        };
        let mut out = Vec::new();
        for ((session, repo_path, wc_target), asked) in
            self.lock_targets(scope)?.into_iter().zip(requested)
        {
            self.network_tick(ctx).await?;
            let ours = match &wc_target {
                Some((root, wc_rel, _)) => self.with_wc(root, |w| {
                    Ok(w.node(wc_rel)?.lock.as_ref().and_then(|l| l.token.clone()))
                })?,
                None => None,
            };
            for l in session
                .get_locks(&repo_path, svn_types::Depth::Empty, &ctx.cancel)
                .await?
            {
                if ours.as_deref() != Some(l.token.as_str()) {
                    out.push(opensvn_ipc::LockConflict {
                        path: asked.clone(),
                        owner: l.owner,
                        comment: l.comment,
                        created_at: Some(timestamp_to_iso(l.created)),
                    });
                }
            }
        }
        Ok(out)
    }

    /// Resolves a lock scope to `(session, repository path, optional (wc root, wc path, rev))`.
    #[allow(clippy::type_complexity)]
    fn lock_targets(
        &self,
        scope: &LockScope,
    ) -> Result<
        Vec<(
            FakeRaSession,
            RelPath,
            Option<(std::path::PathBuf, RelPath, u64)>,
        )>,
        SvnError,
    > {
        match scope {
            LockScope::Urls(urls) => urls
                .iter()
                .map(|u| self.locate(u).map(|(s, p)| (s, p, None)))
                .collect(),
            LockScope::Wc { wc, paths } => paths
                .iter()
                .map(|p| {
                    let rp = rel(p)?;
                    let (root, repo_path, rev) = self.with_wc(&wc.root, |w| {
                        let node = w.node(&rp)?;
                        Ok((w.repo_root_url.clone(), w.repo_path(&rp), node.rev))
                    })?;
                    Ok((
                        self.repo_by_root(&root)?,
                        RelPath::new(repo_path.trim_matches('/'))?,
                        Some((wc.root.clone(), rp, rev)),
                    ))
                })
                .collect(),
        }
    }

    /// Changes that copy the repository subtree at `from` to `to`.
    async fn copy_changes(
        &self,
        session: &FakeRaSession,
        from: &RelPath,
        rev: Option<Revnum>,
        to: &RelPath,
        ctx: &OpContext,
    ) -> Result<Vec<FakeChange>, SvnError> {
        let mut out = Vec::new();
        match session.check_path(from, rev, &ctx.cancel).await? {
            NodeKind::File => {
                let (_, data) = self.read_file(session, from, rev, ctx).await?;
                out.push(FakeChange::PutFile(to.clone(), data.into()));
            }
            NodeKind::Dir => {
                out.push(FakeChange::PutDir(to.clone()));
                let entries = session
                    .list(from, rev, svn_types::Depth::Infinity, &ctx.cancel)
                    .await?;
                for e in entries {
                    let sub = RelPath::from_server(&e.name)?;
                    let target = RelPath::new(&format!("{to}/{sub}"))?;
                    if e.kind == NodeKind::Dir {
                        out.push(FakeChange::PutDir(target));
                    } else {
                        let src = RelPath::new(&format!("{from}/{sub}"))?;
                        let (_, data) = self.read_file(session, &src, rev, ctx).await?;
                        out.push(FakeChange::PutFile(target, data.into()));
                    }
                }
            }
            _ => {
                return Err(SvnError::new(
                    ErrorCode::PathNotFound,
                    format!("/{from} does not exist"),
                ));
            }
        }
        Ok(out)
    }

    async fn commit_remote(
        &self,
        session: &FakeRaSession,
        message: &str,
        changes: Vec<FakeChange>,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        self.network_tick(ctx).await?;
        ctx.check()?;
        let rev = session.seed("tester", message, &changes)?;
        Ok(Rev(rev.0))
    }

    pub(super) async fn do_remote_edit(
        &self,
        req: &RemoteEditPlan,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        let mut session: Option<FakeRaSession> = None;
        let mut changes = Vec::new();
        for op in &req.ops {
            let mut at = |url: &str| -> Result<RelPath, SvnError> {
                let (s, p) = self.locate(url)?;
                match &session {
                    Some(prev) if prev.uuid() != s.uuid() => {
                        Err(internal("remote edit spans several repositories"))
                    }
                    _ => {
                        session = Some(s);
                        Ok(p)
                    }
                }
            };
            match op {
                EditOp::Mkdir { url } => changes.push(FakeChange::PutDir(at(url)?)),
                EditOp::Delete { url } => changes.push(FakeChange::Delete(at(url)?)),
                EditOp::Upload { url, local_path } => {
                    let to = at(url)?;
                    let path = local_path.as_path();
                    let shown = path.display();
                    if !path.is_absolute() {
                        return Err(internal("upload path must be absolute"));
                    }
                    let meta = std::fs::metadata(path)
                        .map_err(|e| SvnError::from_io(format!("read {shown}"), &e))?;
                    if meta.len() > UPLOAD_LIMIT {
                        return Err(internal("file is too large to upload in this build"));
                    }
                    let data = std::fs::read(path)
                        .map_err(|e| SvnError::from_io(format!("read {shown}"), &e))?;
                    changes.push(FakeChange::PutFile(to, Bytes::from(data)));
                }
                EditOp::Copy {
                    from_url,
                    from_rev,
                    to_url,
                } => {
                    let from = at(from_url)?;
                    let to = at(to_url)?;
                    let s = session.clone().ok_or_else(|| internal("no repository"))?;
                    changes.extend(
                        self.copy_changes(&s, &from, revnum(*from_rev), &to, ctx)
                            .await?,
                    );
                }
                EditOp::Rename { from_url, to_url } => {
                    let from = at(from_url)?;
                    let to = at(to_url)?;
                    let s = session.clone().ok_or_else(|| internal("no repository"))?;
                    changes.extend(self.copy_changes(&s, &from, None, &to, ctx).await?);
                    changes.push(FakeChange::Delete(from));
                }
            }
        }
        let session = session.ok_or_else(|| internal("remote edit has no operations"))?;
        session.ensure_authenticated(&ctx.cancel).await?;
        self.commit_remote(&session, &req.message, changes, ctx)
            .await
    }

    pub(super) async fn do_copy_location(
        &self,
        req: &CopyLocationRequest,
        from_wc: Option<&WcRef>,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        let (to_session, to) = self.locate(&req.to_url)?;
        if let Some(wc) = from_wc {
            return self
                .copy_working_copy(wc, &to_session, &to, &req.message, &req.to_url, ctx)
                .await;
        }
        let (session, from) = self.locate(&req.from_url)?;
        if session.uuid() != to_session.uuid() {
            return Err(internal("cannot copy between repositories"));
        }
        session.ensure_authenticated(&ctx.cancel).await?;
        if session.check_path(&to, None, &ctx.cancel).await? != NodeKind::None {
            return Err(internal(format!("{} already exists", req.to_url)));
        }
        let changes = self
            .copy_changes(&session, &from, revnum(req.from_rev), &to, ctx)
            .await?;
        self.commit_remote(&session, &req.message, changes, ctx)
            .await
    }

    /// Copies a working copy, local changes included, to a new repository location.
    async fn copy_working_copy(
        &self,
        wc: &WcRef,
        session: &FakeRaSession,
        to: &RelPath,
        message: &str,
        to_url: &str,
        ctx: &OpContext,
    ) -> Result<Rev, SvnError> {
        let (root, nodes) = self.with_wc(&wc.root, |w| {
            Ok((
                w.repo_root_url.clone(),
                w.nodes
                    .iter()
                    .filter(|(p, n)| {
                        !p.is_root() && matches!(n.sched, Sched::Normal | Sched::Added)
                    })
                    .map(|(p, n)| (p.clone(), n.kind, n.work.clone()))
                    .collect::<Vec<_>>(),
            ))
        })?;
        if self.repo_by_root(&root)?.uuid() != session.uuid() {
            return Err(internal("cannot copy between repositories"));
        }
        session.ensure_authenticated(&ctx.cancel).await?;
        if session.check_path(to, None, &ctx.cancel).await? != NodeKind::None {
            return Err(internal(format!("{to_url} already exists")));
        }
        let mut changes = vec![FakeChange::PutDir(to.clone())];
        for (path, kind, work) in nodes {
            let target = RelPath::new(&format!("{to}/{path}"))?;
            match (kind, work) {
                (opensvn_ipc::FileKind::Dir, _) => changes.push(FakeChange::PutDir(target)),
                (_, Some(content)) => changes.push(FakeChange::PutFile(target, content)),
                _ => {} // a file missing from disk has no content to copy
            }
        }
        self.commit_remote(session, message, changes, ctx).await
    }

    pub(super) async fn do_branches_list(
        &self,
        repo_root_url: &str,
        layout: &Layout,
        ctx: &OpContext,
    ) -> Result<Vec<BranchNode>, SvnError> {
        let (session, root) = self.locate(repo_root_url)?;
        let (trunks, branches, tags): (Vec<String>, Vec<String>, Vec<String>) = match layout {
            Layout::None => return Ok(Vec::new()),
            Layout::Auto => (
                vec!["/trunk".into()],
                vec!["/branches".into()],
                vec!["/tags".into()],
            ),
            Layout::Custom {
                trunk,
                branches,
                tags,
            } => (vec![trunk.clone()], branches.clone(), tags.clone()),
        };
        let projects: Vec<String> = if [&trunks, &branches, &tags]
            .iter()
            .any(|v| v.iter().any(|p| p.contains("{project}")))
        {
            session
                .list(&root, None, svn_types::Depth::Immediates, &ctx.cancel)
                .await?
                .into_iter()
                .filter(|e| e.kind == NodeKind::Dir)
                .map(|e| e.name)
                .collect()
        } else {
            vec![String::new()]
        };
        let base = repo_root_url.trim_end_matches('/');
        let mut out = Vec::new();
        for project in &projects {
            let expand = |p: &str| p.replace("{project}", project).trim_matches('/').to_owned();
            for t in &trunks {
                let path = expand(t);
                if let Ok(rp) = RelPath::new(&path)
                    && session.check_path(&rp, None, &ctx.cancel).await? == NodeKind::Dir
                {
                    out.push(BranchNode {
                        kind: BranchKind::Trunk,
                        name: "trunk".into(),
                        url: format!("{base}/{path}"),
                    });
                }
            }
            for (patterns, kind) in [(&branches, BranchKind::Branch), (&tags, BranchKind::Tag)] {
                for pattern in patterns {
                    let path = expand(pattern);
                    let Ok(rp) = RelPath::new(&path) else {
                        continue;
                    };
                    if session.check_path(&rp, None, &ctx.cancel).await? != NodeKind::Dir {
                        continue;
                    }
                    for e in session
                        .list(&rp, None, svn_types::Depth::Immediates, &ctx.cancel)
                        .await?
                    {
                        if e.kind == NodeKind::Dir {
                            out.push(BranchNode {
                                kind,
                                name: e.name.clone(),
                                url: format!("{base}/{path}/{}", e.name),
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}
