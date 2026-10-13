//! Shared application state handed to every command.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use opensvn_ipc::{AppError, OpResult, OperationHandle, OperationKind, WcId};
use svn_types::SvnError;

use crate::db::{AppDb, CacheDb};
use crate::engine::{EngineFacade, OpContext, WcRef};
use crate::ops::{AppEvents, OperationManager};
use crate::path_grants::{PathGrants, PathPicker};
use crate::programs::{ProgramPicker, ProgramService};
use crate::settings::{SettingsService, system_locale};
use crate::status::StatusPublisher;
use crate::wc::{ResolvedWc, WcRegistry};

/// Everything needed to build the state; tests substitute the engine, events and dialogs.
pub struct AppDeps {
    /// Directory holding `app.sqlite` and `cache.sqlite`.
    pub data_dir: std::path::PathBuf,
    /// Engine implementation.
    pub engine: Arc<dyn EngineFacade>,
    /// Outbound events.
    pub events: Arc<dyn AppEvents>,
    /// Native dialogs for programs.
    pub picker: Arc<dyn ProgramPicker>,
    /// Native dialogs for files and folders.
    pub path_picker: Arc<dyn PathPicker>,
}

/// Services shared by all commands. Managed by Tauri and injected as `State<AppState>`.
pub struct AppState {
    /// Durable app database.
    pub db: AppDb,
    /// Settings with change notification.
    pub settings: Arc<SettingsService>,
    /// Outbound events.
    pub events: Arc<dyn AppEvents>,
    /// Operation lifecycle.
    pub ops: OperationManager,
    /// Engine behind every repository and working copy command.
    pub engine: Arc<dyn EngineFacade>,
    /// Working copy list and actors.
    pub wcs: Arc<WcRegistry>,
    /// Status deltas and snapshots.
    pub status: Arc<StatusPublisher>,
    /// Path grants minted by native dialogs and drops.
    pub grants: Arc<PathGrants>,
    /// Native file and folder dialogs.
    pub path_picker: Arc<dyn PathPicker>,
    /// Serializes native dialogs (a second request cannot stack dialogs).
    pub path_dialog_open: tokio::sync::Mutex<()>,
    /// Program settings (native pick and confirm).
    pub programs: ProgramService,
}

impl AppState {
    /// Opens the databases and wires the services.
    pub fn build(deps: AppDeps) -> Result<Self, AppError> {
        let db = AppDb::open(&deps.data_dir)?;
        let cache = CacheDb::open(&deps.data_dir)?;
        let settings = Arc::new(SettingsService::load(
            db.clone(),
            system_locale().as_deref(),
        )?);
        let wcs = Arc::new(WcRegistry::new(db.clone(), Arc::clone(&deps.engine)));
        let status = Arc::new(StatusPublisher::new(
            Arc::clone(&deps.engine),
            cache,
            Arc::clone(&deps.events),
        ));
        Ok(Self {
            grants: Arc::new(PathGrants::new()),
            path_picker: deps.path_picker,
            path_dialog_open: tokio::sync::Mutex::new(()),
            programs: ProgramService::new(Arc::clone(&settings), deps.picker),
            ops: OperationManager::new(Arc::clone(&deps.events)),
            events: deps.events,
            engine: deps.engine,
            db,
            settings,
            wcs,
            status,
        })
    }

    /// Runs a read on a working copy through its actor (busy-retry applies).
    pub async fn wc_read<T, F, Fut>(&self, id: &WcId, f: F) -> Result<T, AppError>
    where
        F: Fn(Arc<dyn EngineFacade>, WcRef) -> Fut,
        Fut: Future<Output = Result<T, SvnError>>,
    {
        let r = self.wcs.resolve(id)?;
        let engine = Arc::clone(&self.engine);
        r.actor
            .read(|| {
                let fut = f(Arc::clone(&engine), r.wc.clone());
                async move { fut.await.map_err(|e| AppError::from(&e)) }
            })
            .await
    }

    /// Runs a short write on a working copy through its actor and refreshes status afterwards.
    pub async fn wc_write<T, F, Fut>(&self, id: &WcId, f: F) -> Result<T, AppError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn EngineFacade>, WcRef) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, SvnError>> + Send + 'static,
    {
        let r = self.wcs.resolve(id)?;
        let (engine, wc) = (Arc::clone(&self.engine), r.wc.clone());
        let out = r
            .actor
            .write(async move { f(engine, wc).await.map_err(|e| AppError::from(&e)) })
            .await;
        self.publish_quietly(&r).await;
        out
    }

    /// Starts a long write operation on a working copy. The work runs inside the working copy's
    /// actor; status is republished when it ends, also after a failure or cancel, because the
    /// operation may have changed part of the tree.
    pub fn start_wc_op<F, Fut>(
        &self,
        kind: OperationKind,
        id: &WcId,
        title: impl Into<String>,
        f: F,
    ) -> Result<OperationHandle, AppError>
    where
        F: FnOnce(Arc<dyn EngineFacade>, WcRef, OpContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OpResult, SvnError>> + Send + 'static,
    {
        let r = self.wcs.resolve(id)?;
        let (engine, publisher) = (Arc::clone(&self.engine), Arc::clone(&self.status));
        let wc = r.wc.clone();
        Ok(self
            .ops
            .spawn(kind, Some(id.clone()), title, move |ctx| async move {
                let cancel = ctx.cancel.clone();
                let result = r
                    .actor
                    .write_cancellable(
                        async move { f(engine, wc, ctx).await.map_err(|e| AppError::from(&e)) },
                        &cancel,
                    )
                    .await;
                if let Err(e) = publisher.publish(&r).await {
                    tracing::warn!(code = ?e.code, "status refresh after operation failed");
                }
                result
            }))
    }

    /// Starts an operation that needs no working copy (remote edits, downloads, ...).
    pub fn start_op<F, Fut>(
        &self,
        kind: OperationKind,
        title: impl Into<String>,
        f: F,
    ) -> OperationHandle
    where
        F: FnOnce(Arc<dyn EngineFacade>, OpContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OpResult, SvnError>> + Send + 'static,
    {
        let engine = Arc::clone(&self.engine);
        self.ops.spawn(kind, None, title, move |ctx| async move {
            f(engine, ctx).await.map_err(|e| AppError::from(&e))
        })
    }

    /// Opens a working copy (engine recovery included). Locks that need the user (foreign or
    /// unknown owner) are announced as a `needs-cleanup` banner so the UI can offer "Run cleanup";
    /// they are never taken over automatically.
    pub async fn open_wc(&self, id: &WcId) -> Result<opensvn_ipc::WorkingCopy, AppError> {
        let (wc, report) = self.wcs.open(id).await?;
        if !report.recovered.is_empty() {
            tracing::info!(wc = %id.0, dirs = report.recovered.len(), "recovered stale working copy locks");
        }
        if !report.needs_user_action.is_empty() {
            self.events.banner(&opensvn_ipc::Banner {
                wc_id: id.clone(),
                kind: opensvn_ipc::BannerKind::NeedsCleanup,
                data: report.needs_user_action,
            });
        }
        Ok(wc)
    }

    /// Runs recovery for every registered working copy (app start). A failure is logged and does
    /// not stop the others.
    pub async fn recover_all(&self) {
        for id in self.wcs.ids().unwrap_or_default() {
            if let Err(e) = self.open_wc(&id).await {
                tracing::warn!(wc = %id.0, code = ?e.code, "working copy recovery failed");
            }
        }
    }

    /// Turns a native drop into grants and announces them as `fs://dropped`. The raw paths never
    /// reach the WebView; only opaque ids and display names do.
    pub fn handle_drop(&self, paths: &[std::path::PathBuf], x: f64, y: f64) {
        if paths.is_empty() {
            return;
        }
        let grants = paths
            .iter()
            .map(|p| {
                let kind = if p.is_dir() {
                    opensvn_ipc::GrantKind::Dir
                } else {
                    opensvn_ipc::GrantKind::File
                };
                self.grants
                    .mint(p.clone(), kind, crate::path_grants::MAIN_WINDOW)
            })
            .collect();
        // Rounded logical coordinates; the cast saturates for out-of-range values.
        #[allow(clippy::cast_possible_truncation)]
        let position = opensvn_ipc::DropPosition {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        self.events
            .dropped(&opensvn_ipc::DroppedPaths { grants, position });
    }

    /// Redeems a folder grant from the `main` window.
    pub fn redeem_dir(
        &self,
        id: &opensvn_ipc::PathGrantId,
    ) -> Result<std::path::PathBuf, AppError> {
        self.grants.redeem(
            id,
            crate::path_grants::MAIN_WINDOW,
            opensvn_ipc::GrantKind::Dir,
        )
    }

    /// Redeems a file grant from the `main` window.
    pub fn redeem_file(
        &self,
        id: &opensvn_ipc::PathGrantId,
    ) -> Result<std::path::PathBuf, AppError> {
        self.grants.redeem(
            id,
            crate::path_grants::MAIN_WINDOW,
            opensvn_ipc::GrantKind::File,
        )
    }

    /// Republishes status; a failure is logged, never surfaced (the caller's result matters more).
    pub async fn publish_quietly(&self, r: &ResolvedWc) {
        if let Err(e) = self.status.publish(r).await {
            tracing::warn!(code = ?e.code, "status refresh failed");
        }
    }
}

/// Last path component for operation titles.
pub fn leaf(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}
