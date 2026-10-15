//! The shared scenario `open_recovers_dead_locks_and_reports_foreign_ones` against the real
//! engine's recovery, on a working copy checked out by `svn-client`.
//!
//! The engine's own checkout is wired separately (`sync.rs`); until then [`ClientCheckout`]
//! performs the checkout of the scenario with `svn_client::checkout` and hands every other call
//! to the real engine unchanged, so the scenario exercises the real `open_wc` recovery.

use std::path::Path;

use async_trait::async_trait;
use opensvn_ipc::{
    BlameLine, BranchNode, CleanupRequest, CommitRequest, CopyLocationRequest, Depth,
    EngineCapabilities, FileDiff, FileStatus, IgnoreAddRequest, Layout, LogGetRequest, LogPage,
    RepoInfo, RepoListing, ResolveRequest, Rev, RevertRequest, TextSide, UpdateRequest,
};
use svn_types::{CancelToken, ErrorCode, RepoUrl, Revnum, SvnError};

use super::support::{Fixture, fixture, plant_dead_app_lock, plant_lock_row};
use crate::engine::conformance::{self, Harness, PlantedOwner};
use crate::engine::{
    CheckoutPlan, DiffOptions, EngineFacade, EngineResult, ExportedSides, LockScope, LogScope,
    OpContext, RemoteEditPlan, RemoteScan, SideFiles, StatusQuery, WcInfo, WcOpened, WcRef,
};

/// The real engine with checkout done by `svn-client` directly.
struct ClientCheckout(Fixture);

#[async_trait]
impl EngineFacade for ClientCheckout {
    fn capabilities(&self) -> EngineCapabilities {
        self.0.engine.capabilities()
    }
    async fn wc_info(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcInfo> {
        self.0.engine.wc_info(root, ctx).await
    }
    async fn ensure_authenticated(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<()> {
        self.0.engine.ensure_authenticated(wc, ctx).await
    }
    async fn open_wc(&self, root: &Path, ctx: &OpContext) -> EngineResult<WcOpened> {
        self.0.engine.open_wc(root, ctx).await
    }
    async fn status(
        &self,
        wc: &WcRef,
        query: StatusQuery,
        ctx: &OpContext,
    ) -> EngineResult<Vec<FileStatus>> {
        self.0.engine.status(wc, query, ctx).await
    }
    async fn diff_wc(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        self.0.engine.diff_wc(wc, path, against, opts, ctx).await
    }

    async fn export_wc_sides(
        &self,
        wc: &WcRef,
        path: &str,
        against: Option<Rev>,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> EngineResult<ExportedSides> {
        self.0
            .engine
            .export_wc_sides(wc, path, against, to, ctx)
            .await
    }
    async fn changelist_set(
        &self,
        wc: &WcRef,
        paths: &[String],
        name: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.changelist_set(wc, paths, name, ctx).await
    }
    async fn ignore_add(
        &self,
        wc: &WcRef,
        req: &IgnoreAddRequest,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.ignore_add(wc, req, ctx).await
    }
    async fn mark_move(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.mark_move(wc, from, to, ctx).await
    }
    async fn resolve(&self, wc: &WcRef, req: &ResolveRequest, ctx: &OpContext) -> EngineResult<()> {
        self.0.engine.resolve(wc, req, ctx).await
    }
    async fn props_set(
        &self,
        wc: &WcRef,
        path: &str,
        name: &str,
        value: Option<&str>,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.props_set(wc, path, name, value, ctx).await
    }
    async fn checkout(&self, plan: &CheckoutPlan, ctx: &OpContext) -> EngineResult<WcInfo> {
        let url = RepoUrl::parse(&plan.url)?;
        let session = self.0.repo.at(&url)?;
        let depth = match plan.depth {
            Depth::Infinity => svn_types::Depth::Infinity,
            _ => return Err(SvnError::new(ErrorCode::Internal, "test checkout: depth")),
        };
        svn_client::checkout(
            &self.0.engine.client_ctx(ctx),
            &session,
            &plan.path,
            plan.rev.map(|r| Revnum(r.0)),
            depth,
            false,
            &CancelToken::new(),
        )
        .await?;
        self.0.engine.wc_info(&plan.path, ctx).await
    }
    async fn status_remote(&self, wc: &WcRef, ctx: &OpContext) -> EngineResult<RemoteScan> {
        self.0.engine.status_remote(wc, ctx).await
    }
    async fn update(&self, wc: &WcRef, req: &UpdateRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.0.engine.update(wc, req, ctx).await
    }
    async fn switch(
        &self,
        wc: &WcRef,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        self.0.engine.switch(wc, url, rev, ctx).await
    }
    async fn commit(&self, wc: &WcRef, req: &CommitRequest, ctx: &OpContext) -> EngineResult<Rev> {
        self.0.engine.commit(wc, req, ctx).await
    }
    async fn revert(&self, wc: &WcRef, req: &RevertRequest, ctx: &OpContext) -> EngineResult<()> {
        self.0.engine.revert(wc, req, ctx).await
    }
    async fn add_paths(&self, wc: &WcRef, paths: &[String], ctx: &OpContext) -> EngineResult<()> {
        self.0.engine.add_paths(wc, paths, ctx).await
    }
    async fn delete_paths(
        &self,
        wc: &WcRef,
        paths: &[String],
        keep_local: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.delete_paths(wc, paths, keep_local, ctx).await
    }
    async fn move_path(
        &self,
        wc: &WcRef,
        from: &str,
        to: &str,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.move_path(wc, from, to, ctx).await
    }
    async fn cleanup(&self, wc: &WcRef, req: &CleanupRequest, ctx: &OpContext) -> EngineResult<()> {
        self.0.engine.cleanup(wc, req, ctx).await
    }
    async fn set_depth(
        &self,
        wc: &WcRef,
        path: &str,
        depth: Depth,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.set_depth(wc, path, depth, ctx).await
    }
    async fn upgrade_format(&self, wc: &WcRef, to: u8, ctx: &OpContext) -> EngineResult<()> {
        self.0.engine.upgrade_format(wc, to, ctx).await
    }
    async fn repo_info_at(
        &self,
        url: &str,
        peg: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<RepoInfo> {
        self.0.engine.repo_info_at(url, peg, ctx).await
    }
    async fn repo_list(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<RepoListing> {
        self.0.engine.repo_list(url, rev, ctx).await
    }
    async fn repo_cat(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<TextSide> {
        self.0.engine.repo_cat(url, rev, ctx).await
    }
    async fn repo_save(
        &self,
        url: &str,
        rev: Option<Rev>,
        save_to: &Path,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.repo_save(url, rev, save_to, ctx).await
    }
    async fn diff_rev(
        &self,
        url: &str,
        from: Option<Rev>,
        to: Rev,
        opts: DiffOptions,
        ctx: &OpContext,
    ) -> EngineResult<FileDiff> {
        self.0.engine.diff_rev(url, from, to, opts, ctx).await
    }

    async fn export_rev_sides(
        &self,
        url: &str,
        from: Option<Rev>,
        to_rev: Rev,
        to: SideFiles<'_>,
        ctx: &OpContext,
    ) -> EngineResult<ExportedSides> {
        self.0
            .engine
            .export_rev_sides(url, from, to_rev, to, ctx)
            .await
    }
    async fn log(
        &self,
        scope: &LogScope,
        req: &LogGetRequest,
        ctx: &OpContext,
    ) -> EngineResult<LogPage> {
        self.0.engine.log(scope, req, ctx).await
    }
    async fn blame(
        &self,
        url: &str,
        rev: Option<Rev>,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BlameLine>> {
        self.0.engine.blame(url, rev, ctx).await
    }
    async fn lock(
        &self,
        scope: &LockScope,
        comment: Option<&str>,
        steal: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.lock(scope, comment, steal, ctx).await
    }
    async fn unlock(
        &self,
        scope: &LockScope,
        break_lock: bool,
        ctx: &OpContext,
    ) -> EngineResult<()> {
        self.0.engine.unlock(scope, break_lock, ctx).await
    }
    async fn lock_conflicts(
        &self,
        scope: &LockScope,
        ctx: &OpContext,
    ) -> EngineResult<Vec<opensvn_ipc::LockConflict>> {
        self.0.engine.lock_conflicts(scope, ctx).await
    }
    async fn remote_edit(&self, plan: &RemoteEditPlan, ctx: &OpContext) -> EngineResult<Rev> {
        self.0.engine.remote_edit(plan, ctx).await
    }
    async fn copy_location(
        &self,
        req: &CopyLocationRequest,
        from_wc: Option<&WcRef>,
        ctx: &OpContext,
    ) -> EngineResult<Rev> {
        self.0.engine.copy_location(req, from_wc, ctx).await
    }
    async fn branches_list(
        &self,
        repo_root_url: &str,
        layout: &Layout,
        ctx: &OpContext,
    ) -> EngineResult<Vec<BranchNode>> {
        self.0
            .engine
            .branches_list(repo_root_url, layout, ctx)
            .await
    }
}

struct RecoveryHarness {
    engine: ClientCheckout,
    scratch: tempfile::TempDir,
}

impl Harness for RecoveryHarness {
    fn engine(&self) -> &dyn EngineFacade {
        &self.engine
    }
    fn repo_url(&self) -> String {
        super::support::URL.to_owned()
    }
    fn work_dir(&self) -> std::path::PathBuf {
        self.scratch.path().join("work")
    }
    fn write_file(&self, root: &Path, rel: &str, content: &str) -> Result<(), SvnError> {
        std::fs::write(root.join(rel), content).map_err(|e| SvnError::from_io("write", &e))
    }
    fn remove_file(&self, root: &Path, rel: &str) -> Result<(), SvnError> {
        std::fs::remove_file(root.join(rel)).map_err(|e| SvnError::from_io("remove", &e))
    }
    fn scratch_dir(&self) -> std::path::PathBuf {
        self.scratch.path().to_path_buf()
    }
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError> {
        match owner {
            PlantedOwner::DeadProcess => plant_dead_app_lock(root, &self.engine.0.store, dir),
            PlantedOwner::Foreign => plant_lock_row(root, dir),
        }
        Ok(())
    }
}

#[tokio::test]
async fn shared_suite_open_recovers_dead_locks_and_reports_foreign_ones() {
    let h = RecoveryHarness {
        engine: ClientCheckout(fixture()),
        scratch: tempfile::tempdir().unwrap(),
    };
    conformance::open_recovers_dead_locks_and_reports_foreign_ones(&h).await;
}
