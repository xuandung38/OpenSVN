//! In-memory engine used until the real engine is registered, so the whole UI runs end to end.
//!
//! Repositories are `FakeRaSession`s from `svn-ra-api`; working copies live in an in-memory model
//! (nothing is written to disk except by `repo_save`). It implements every call of
//! [`EngineFacade`] with believable behavior: scheduling, conflicts on update, mixed revisions
//! after commit, locks, the two cancellation point kinds and throttled-friendly progress.

#[cfg(test)]
mod conformance_tests;
mod local_ops;
mod remote_ops;
mod seed;
mod side_export;
mod state;
mod sync_ops;
#[cfg(test)]
mod tick_tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use opensvn_ipc::{
    BlameLine, BranchNode, CleanupRequest, CommitRequest, CopyLocationRequest, Depth,
    EngineCapabilities, FileDiff, FileStatus, IgnoreAddRequest, Layout, LogGetRequest, LogPage,
    RepoEntry, RepoInfo, ResolveRequest, Rev, RevertRequest, TextSide, UpdateRequest,
};
use svn_ra_api::RaSession;
use svn_ra_api::fake::FakeRaSession;
use svn_types::{ErrorCode, RelPath, SvnError};

use self::state::FakeWc;
use super::context::OpContext;
use super::facade::{
    CheckoutPlan, DiffOptions, EngineFacade, EngineResult, ExportedSides, LockScope, LogScope,
    RecoveryReport, RemoteEditPlan, RemoteScan, SideFiles, StatusQuery, WcInfo, WcOpened, WcRef,
};

pub use seed::{DEMO_REPO_URL, demo_repo, demo_wc_root};

/// Fake engine; cheap to share behind an `Arc`.
pub struct FakeEngine {
    wcs: Mutex<HashMap<PathBuf, FakeWc>>,
    repos: Vec<FakeRaSession>,
    /// Pause per simulated item, so progress and cancellation are observable in the UI.
    step_delay: Duration,
}

impl FakeEngine {
    /// Engine without repositories or working copies, no artificial delay.
    pub fn empty() -> Self {
        Self {
            wcs: Mutex::new(HashMap::new()),
            repos: Vec::new(),
            step_delay: Duration::ZERO,
        }
    }

    /// Adds a repository that URLs below its root resolve to.
    #[must_use]
    pub fn with_repo(mut self, repo: FakeRaSession) -> Self {
        self.repos.push(repo);
        self
    }

    /// Sets the simulated time per item of long operations.
    #[must_use]
    pub fn with_step_delay(mut self, delay: Duration) -> Self {
        self.step_delay = delay;
        self
    }

    /// Runs `f` on the working copy rooted at `root`.
    fn with_wc<T>(
        &self,
        root: &Path,
        f: impl FnOnce(&mut FakeWc) -> Result<T, SvnError>,
    ) -> Result<T, SvnError> {
        let mut wcs = self.wcs.lock().unwrap_or_else(PoisonError::into_inner);
        let wc = wcs.get_mut(root).ok_or_else(|| not_a_wc(root))?;
        f(wc)
    }

    /// Finds the repository containing `url` and the path of `url` inside it.
    fn locate(&self, url: &str) -> Result<(FakeRaSession, RelPath), SvnError> {
        let parsed = svn_types::RepoUrl::parse(url)?;
        for repo in &self.repos {
            let root = repo.repos_root().to_string();
            let Some(rest) = parsed.to_string().strip_prefix(&root).map(str::to_owned) else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with('/') {
                continue; // `https://h/repo2` is not below `https://h/repo`
            }
            let decoded = super::text::percent_decode(rest.trim_matches('/'));
            return Ok((repo.clone(), RelPath::new(&decoded)?));
        }
        // Like an `SVNParentPath` location (VisualSVN Server's `https://host/svn/`): the folder
        // that holds repositories lists them instead of being one.
        let here = parsed.to_string();
        let repos: Vec<svn_types::ParentRepo> = self
            .repos
            .iter()
            .filter_map(|repo| {
                let root = repo.repos_root().to_string();
                let (parent, name) = root.rsplit_once('/')?;
                (parent == here).then(|| svn_types::ParentRepo {
                    name: super::text::percent_decode(name),
                    url: root.clone(),
                })
            })
            .collect();
        if !repos.is_empty() {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!(
                    "{here} is the list of repositories on this server, not a repository; add a \
                     repository name to the URL"
                ),
            )
            .with_repositories(repos));
        }
        Err(SvnError::new(
            ErrorCode::Network,
            format!("cannot connect to {url}: unknown repository"),
        ))
    }

    /// Repository with this root URL.
    fn repo_by_root(&self, root: &str) -> Result<FakeRaSession, SvnError> {
        self.locate(root).map(|(r, _)| r)
    }

    /// Simulated network step: ends early with `Cancelled` when the user cancels.
    async fn network_tick(&self, ctx: &OpContext) -> Result<(), SvnError> {
        ctx.check()?;
        if self.step_delay.is_zero() {
            tokio::task::yield_now().await;
            return ctx.check();
        }
        // Biased: a cancel that lands together with the end of the step must win, or the caller
        // would start the next item after the user saw "cancelled".
        tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Err(super::context::cancelled()),
            () = tokio::time::sleep(self.step_delay) => Ok(()),
        }
    }

    /// Simulated work-queue step: the cancel check happens before the item starts, never during
    /// it, mirroring "cancel only between work queue items".
    async fn queue_tick(&self, ctx: &OpContext) -> Result<(), SvnError> {
        ctx.check()?;
        if self.step_delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(self.step_delay).await;
        }
        Ok(())
    }
}

fn not_a_wc(root: &Path) -> SvnError {
    SvnError::new(
        ErrorCode::NotAWorkingCopy,
        format!("{} is not a working copy", root.display()),
    )
}

#[async_trait]
impl EngineFacade for FakeEngine {
    fn capabilities(&self) -> EngineCapabilities {
        // The fake accepts `ignore_externals` (it has no externals to fetch either way).
        // `demo` is masked in debug builds by the `engine_capabilities` command.
        EngineCapabilities {
            externals: true,
            demo: true,
        }
    }

    async fn wc_info(&self, root: &Path, _ctx: &OpContext) -> EngineResult<WcInfo> {
        self.with_wc(root, |wc| Ok(wc.info()))
    }

    async fn ensure_authenticated(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<()> {
        if !ctx.interactive {
            return Ok(()); // background work never prompts
        }
        let root = self.with_wc(&wc.root, |w| Ok(w.repo_root_url.clone()))?;
        self.repo_by_root(&root)?
            .ensure_authenticated(&ctx.cancel)
            .await
    }

    async fn open_wc(&self, root: &Path, _ctx: &OpContext) -> EngineResult<WcOpened> {
        self.with_wc(root, |wc| {
            let recovered = std::mem::take(&mut wc.dead_owner_locks);
            Ok(WcOpened {
                info: wc.info(),
                recovery: RecoveryReport {
                    recovered,
                    needs_user_action: wc.foreign_locks.clone(),
                },
            })
        })
    }

    async fn status(
        &self,
        wc: &WcRef,
        query: StatusQuery,
        _ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>> {
        self.with_wc(&wc.root, |w| Ok(w.statuses(query)))
    }

    async fn diff_wc(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        self.do_diff_wc(wc, path, against, opts, ctx).await
    }

    async fn export_wc_sides(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> EngineResult<ExportedSides> {
        self.do_export_wc_sides(wc, path, against, to, ctx).await
    }

    async fn changelist_set(
        &self,
        wc: &WcRef,
        paths: &[String],
        name: Option<&str>,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_changelist_set(&wc.root, paths, name)
    }

    async fn ignore_add(
        &self,
        wc: &WcRef,
        req: &IgnoreAddRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_ignore_add(&wc.root, req)
    }

    async fn mark_move(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_mark_move(&wc.root, from, to)
    }

    async fn resolve(
        &self,
        wc: &WcRef,
        req: &ResolveRequest,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_resolve(&wc.root, req)
    }

    async fn props_set(
        &self,
        wc: &WcRef,
        path: &str,
        name: &str,
        value: Option<&str>,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_props_set(&wc.root, path, name, value)
    }

    async fn checkout(&self, plan: &CheckoutPlan, ctx: &OpContext) -> EngineResult<WcInfo> {
        self.do_checkout(plan, ctx).await
    }

    async fn status_remote(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<RemoteScan> {
        self.do_status_remote(wc, ctx).await
    }

    async fn update(&self, wc: &WcRef, req: &UpdateRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_update(wc, req, ctx).await
    }

    async fn switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ignore_ancestry: bool,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        self.do_switch(wc, url, rev, ignore_ancestry, ctx).await
    }

    async fn commit(&self, wc: &WcRef, req: &CommitRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_commit(wc, req, ctx).await
    }

    async fn revert(&self, wc: &WcRef, req: &RevertRequest, ctx: &OpContext) -> EngineResult<()> {
        ctx.check()?;
        self.do_revert(&wc.root, req)
    }

    async fn add_paths(&self, wc: &WcRef, paths: &[String], ctx: &OpContext) -> EngineResult<()> {
        ctx.check()?;
        self.do_add(&wc.root, paths)
    }

    async fn delete_paths(
        &self,
        wc: &WcRef,
        paths: &[String],
        keep_local: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        self.do_delete(&wc.root, paths, keep_local)
    }

    async fn move_path(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        self.do_move(&wc.root, from, to)
    }

    async fn cleanup(&self, wc: &WcRef, req: &CleanupRequest, ctx: &OpContext) -> EngineResult<()> {
        ctx.check()?;
        self.do_cleanup(&wc.root, req)
    }

    async fn set_depth(
        &self,
        wc: &WcRef,
        path: &str,
        depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        ctx.check()?;
        self.do_set_depth(&wc.root, path, depth)
    }

    async fn upgrade_format(&self, wc: &WcRef, to: u8, ctx: &OpContext) -> EngineResult<()> {
        ctx.check()?;
        self.do_upgrade(&wc.root, to)
    }

    async fn repo_info_at(
        &self,
        url: &str,
        peg: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<RepoInfo> {
        self.do_repo_info(url, peg, ctx).await
    }

    async fn repo_list(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<RepoEntry>> {
        self.do_repo_list(url, rev, ctx).await
    }

    async fn repo_cat(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<TextSide> {
        self.do_repo_cat(url, rev, ctx).await
    }

    async fn repo_save(
        &self,
        url: &str,
        rev: Option<Rev>,
        save_to: &Path,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_repo_save(url, rev, save_to, ctx).await
    }

    async fn diff_rev(
        &self,
        url: &str,
        from: Option<Rev>,
        to: Rev,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        self.do_diff_rev(url, from, to, opts, ctx).await
    }

    async fn export_rev_sides(
        &self,
        url: &str,
        from: Option<Rev>,
        to_rev: Rev,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> EngineResult<ExportedSides> {
        self.do_export_rev_sides(url, from, to_rev, to, ctx).await
    }

    async fn log(
        &self,
        scope: &LogScope,
        req: &LogGetRequest,
        ctx: &OpContext,
    ) -> EngineResult<LogPage> {
        self.do_log(scope, req, ctx).await
    }

    async fn blame(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BlameLine>> {
        self.do_blame(url, rev, ctx).await
    }

    async fn lock(
        &self,
        scope: &LockScope,
        comment: Option<&str>,
        steal: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_lock(scope, comment, steal, ctx).await
    }

    async fn unlock(
        &self,
        scope: &LockScope,
        break_lock: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.do_unlock(scope, break_lock, ctx).await
    }

    async fn lock_conflicts(
        &self,
        scope: &LockScope,
        ctx: &OpContext,
    ) -> EngineResult<Vec<opensvn_ipc::LockConflict>> {
        self.do_lock_conflicts(scope, ctx).await
    }

    async fn remote_edit(&self, plan: &RemoteEditPlan, ctx: &OpContext) -> EngineResult<Rev> {
        self.do_remote_edit(plan, ctx).await
    }

    async fn copy_location(
        &self,
        req: &CopyLocationRequest,
        from_wc: Option<&WcRef>,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        self.do_copy_location(req, from_wc, ctx).await
    }

    async fn branches_list(
        &self,
        repo_root_url: &str,
        layout: &Layout,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BranchNode>> {
        self.do_branches_list(repo_root_url, layout, ctx).await
    }
}
