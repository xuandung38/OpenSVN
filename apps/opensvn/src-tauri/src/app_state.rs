//! Shared application state handed to every command.

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};

use opensvn_ipc::{AppError, OpResult, OperationHandle, OperationKind, WcId};
use svn_types::SvnError;

use crate::auth::AuthBroker;
use crate::db::{AppDb, CacheDb};
use crate::diagnostics::DiagnosticsService;
use crate::engine::{EngineFacade, OpContext, WcRef};
use crate::menu::MenuControl;
use crate::notify::Notifier;
use crate::ops::{AppEvents, OperationManager};
use crate::os_integration::OsOpener;
use crate::path_grants::{PathGrants, PathPicker};
use crate::programs::{ProgramLauncher, ProgramPicker, ProgramService};
use crate::settings::{SettingsService, system_locale};
use crate::status::{IncomingPoller, PollerDeps, StatusPublisher, StatusService, StatusTiming};
use crate::updater::{SecretStore, UpdateControl};
use crate::wc::{ResolvedWc, WcRegistry};

/// Services an engine may need, created before it (decision C of the real engine wiring plan):
/// the broker must exist so the engine's transport can prompt and read saved credentials.
#[derive(Clone)]
// Only the real engine's builder reads every field.
#[cfg_attr(not(feature = "real-engine"), allow(dead_code))]
pub struct EngineServices {
    /// Credential and certificate prompts, saved credentials and trusted certificates.
    pub auth: Arc<AuthBroker>,
    /// Durable app database.
    pub db: AppDb,
    /// Data directory (databases, logs, engine state).
    pub data_dir: std::path::PathBuf,
    /// Settings with change notification (network settings of the transport).
    pub settings: Arc<SettingsService>,
}

/// Builds the engine from the app services.
pub type EngineBuilder =
    Box<dyn FnOnce(EngineServices) -> Result<Arc<dyn EngineFacade>, AppError> + Send>;

/// Where the engine comes from.
pub enum EngineSource {
    /// An engine that needs nothing from the app (the fake engine, tests).
    Ready(Arc<dyn EngineFacade>),
    /// An engine built once the broker, database and settings exist (the real engine).
    // Constructed only by `real_engine_source` (feature `real-engine`) and tests.
    #[cfg_attr(not(any(test, feature = "real-engine")), allow(dead_code))]
    Build(EngineBuilder),
}

/// Folder below the data directory for the real engine's per-user state (commit journals).
#[cfg(feature = "real-engine")]
pub const ENGINE_STATE_SUBDIR: &str = "state";

/// The real engine over the HTTP transport, built once the broker, database and settings exist.
#[cfg(feature = "real-engine")]
pub fn real_engine_source() -> EngineSource {
    EngineSource::Build(Box::new(|services| {
        let engine: Arc<dyn EngineFacade> = build_real_engine(&services)?;
        Ok(engine)
    }))
}

/// Builds [`RealEngine`](crate::engine::real::RealEngine): sessions through one
/// [`HttpConnector`](crate::engine::real::HttpConnector) over the broker, working copy lock
/// owners in `app.sqlite`, state below `<data_dir>/state`, and svn's runtime configuration read
/// while `wc.readSubversionConfig` is on.
#[cfg(feature = "real-engine")]
pub(crate) fn build_real_engine(
    services: &EngineServices,
) -> Result<Arc<crate::engine::real::RealEngine>, AppError> {
    use crate::engine::real::{ClientRecovery, HttpConnector, RealEngine, RealEngineConfig};
    use crate::lock_ownership::{SqliteLockOwnership, current_boot_id};

    // Private to the user: commit journals hold log messages and paths.
    let state_dir = crate::preview::prepare_private_dir(&services.data_dir, ENGINE_STATE_SUBDIR)?;
    let engine = Arc::new(RealEngine::new(RealEngineConfig {
        connector: Arc::new(HttpConnector::from_services(services)),
        lock_ownership: Arc::new(SqliteLockOwnership::with_os_probe(services.db.clone())),
        boot_id: current_boot_id().to_owned(),
        state_dir,
        recovery: Arc::new(ClientRecovery),
    }));
    follow_subversion_config_setting(&engine, &services.settings);
    Ok(engine)
}

/// Points the engine at svn's per-user configuration directory while `wc.readSubversionConfig`
/// is on (built-in defaults while it is off), now and after every change of the setting. The
/// follower stops when the engine or the settings service is dropped.
#[cfg(feature = "real-engine")]
fn follow_subversion_config_setting(
    engine: &Arc<crate::engine::real::RealEngine>,
    settings: &SettingsService,
) {
    fn apply(engine: &crate::engine::real::RealEngine, read: bool) {
        engine.set_subversion_config_dir(if read {
            crate::engine::real::default_subversion_config_dir()
        } else {
            None
        });
    }
    let mut rx = settings.subscribe();
    let mut current = rx.borrow_and_update().wc_read_subversion_config;
    apply(engine, current);
    let weak = Arc::downgrade(engine);
    tauri::async_runtime::spawn(async move {
        while rx.changed().await.is_ok() {
            let read = rx.borrow_and_update().wc_read_subversion_config;
            let Some(engine) = weak.upgrade() else { break };
            if read != current {
                current = read;
                apply(&engine, read);
            }
        }
    });
}

/// Everything needed to build the state; tests substitute the engine, events and dialogs.
pub struct AppDeps {
    /// Directory holding `app.sqlite` and `cache.sqlite`.
    pub data_dir: std::path::PathBuf,
    /// Engine implementation.
    pub engine: EngineSource,
    /// Store of saved repository passwords (the OS credential store in the app).
    pub credentials: Arc<dyn secure_store::CredentialStore>,
    /// Outbound events.
    pub events: Arc<dyn AppEvents>,
    /// Native dialogs for programs.
    pub picker: Arc<dyn ProgramPicker>,
    /// Watcher timing.
    pub status_timing: StatusTiming,
    /// Native dialogs for files and folders.
    pub path_picker: Arc<dyn PathPicker>,
    /// Reveal / open launcher.
    pub opener: Arc<dyn OsOpener>,
    /// Desktop notifications.
    pub notifier: Arc<dyn Notifier>,
    /// Native menu control.
    pub menu: Arc<dyn MenuControl>,
    /// App updates.
    pub updates: Arc<dyn UpdateControl>,
    /// Credential store (updater token).
    pub secrets: Arc<dyn SecretStore>,
    /// Starts external programs (the diff tool).
    pub launcher: Arc<dyn ProgramLauncher>,
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
    /// Credential and certificate prompts.
    pub auth: Arc<AuthBroker>,
    /// Diagnostics preview and export.
    pub diagnostics: DiagnosticsService,
    /// File watchers of open working copies.
    pub watchers: Arc<StatusService>,
    /// Periodic update check (started after the window exists).
    pub poller: Mutex<Option<IncomingPoller>>,
    /// Path grants minted by native dialogs and drops.
    pub grants: Arc<PathGrants>,
    /// Native file and folder dialogs.
    pub path_picker: Arc<dyn PathPicker>,
    /// Reveal / open launcher.
    pub opener: Arc<dyn OsOpener>,
    /// Desktop notifications.
    pub notifier: Arc<dyn Notifier>,
    /// Native menu control.
    pub menu: Arc<dyn MenuControl>,
    /// App updates.
    pub updates: Arc<dyn UpdateControl>,
    /// Credential store (updater token).
    pub secrets: Arc<dyn SecretStore>,
    /// Serializes native dialogs (a second request cannot stack dialogs).
    pub path_dialog_open: tokio::sync::Mutex<()>,
    /// Program settings (native pick and confirm).
    pub programs: ProgramService,
    /// Starts external programs (the diff tool).
    pub launcher: Arc<dyn ProgramLauncher>,
    /// Data directory (databases, logs); private folders for generated files live under it.
    pub data_dir: std::path::PathBuf,
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
        let auth = Arc::new(AuthBroker::new(
            Arc::clone(&deps.events),
            db.clone(),
            deps.credentials,
        ));
        let engine = match deps.engine {
            EngineSource::Ready(engine) => engine,
            EngineSource::Build(build) => build(EngineServices {
                auth: Arc::clone(&auth),
                db: db.clone(),
                data_dir: deps.data_dir.clone(),
                settings: Arc::clone(&settings),
            })?,
        };
        let wcs = Arc::new(WcRegistry::new(db.clone(), Arc::clone(&engine)));
        let status = Arc::new(StatusPublisher::new(
            Arc::clone(&engine),
            cache,
            Arc::clone(&deps.events),
        ));
        let watchers = Arc::new(StatusService::new(Arc::clone(&status), deps.status_timing));
        Ok(Self {
            diagnostics: DiagnosticsService::default(),
            opener: deps.opener,
            notifier: deps.notifier,
            menu: deps.menu,
            updates: deps.updates,
            secrets: deps.secrets,
            poller: Mutex::new(None),
            auth,
            watchers,
            grants: Arc::new(PathGrants::new()),
            path_picker: deps.path_picker,
            path_dialog_open: tokio::sync::Mutex::new(()),
            programs: ProgramService::new(Arc::clone(&settings), deps.picker),
            launcher: deps.launcher,
            data_dir: deps.data_dir,
            ops: OperationManager::new(Arc::clone(&deps.events)),
            events: deps.events,
            engine,
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
                // Authenticate first: a prompt must never wait while the working copy is locked.
                engine
                    .ensure_authenticated(&wc, &ctx)
                    .await
                    .map_err(|e| AppError::from(&e))?;
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

    /// Starts the periodic update check, replacing a running one. `period` overrides the setting
    /// (tests).
    pub fn start_poller(&self, period: Option<std::time::Duration>) {
        let poller = IncomingPoller::start(PollerDeps {
            engine: Arc::clone(&self.engine),
            publisher: Arc::clone(&self.status),
            wcs: Arc::clone(&self.wcs),
            notifier: Arc::clone(&self.notifier),
            settings: self.settings.subscribe(),
            period_override: period,
        });
        let mut slot = self
            .poller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = slot.replace(poller) {
            old.stop();
        }
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
