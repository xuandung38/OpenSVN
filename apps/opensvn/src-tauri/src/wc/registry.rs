//! Registry of working copies: persisted list plus one [`WcActor`] per working copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use opensvn_ipc::{AppError, BranchRef, ErrorCode, Layout, WcEntry, WcId, WorkingCopy};

use super::actor::{RetryPolicy, WcActor};
use crate::db::{AppDb, WcRow, now_iso};
use crate::engine::{EngineFacade, OpContext, RecoveryReport, WcInfo, WcRef};
use crate::ops::new_op_id;

/// Runtime facts about a working copy that are not persisted.
#[derive(Default, Clone)]
struct Live {
    incoming: u32,
    last_checked_at: Option<String>,
    branch: Option<CachedBranch>,
}

/// Branch detected for one URL of a working copy. A switch changes the URL, which invalidates it.
#[derive(Clone)]
struct CachedBranch {
    url: String,
    branch: Option<BranchRef>,
    /// Set when detection failed (offline, no credentials): retried after [`BRANCH_RETRY`].
    failed_at: Option<Instant>,
}

/// How long a failed branch detection is remembered before the server is asked again.
const BRANCH_RETRY: Duration = Duration::from_secs(60);
/// Upper bound on the server round trips of one branch detection.
const BRANCH_TIMEOUT: Duration = Duration::from_secs(5);

/// A working copy resolved for use: engine reference, actor and registry row.
#[derive(Clone)]
pub struct ResolvedWc {
    /// Engine-facing reference.
    pub wc: WcRef,
    /// Serializes access to this working copy.
    pub actor: Arc<WcActor>,
    /// Persisted row.
    pub row: WcRow,
}

/// Owns the working copy list and the actors. Cheap to share behind an `Arc`.
pub struct WcRegistry {
    db: AppDb,
    engine: Arc<dyn EngineFacade>,
    policy: RetryPolicy,
    actors: Mutex<HashMap<String, Arc<WcActor>>>,
    live: Mutex<HashMap<String, Live>>,
    open: Mutex<std::collections::BTreeSet<String>>,
}

fn unknown_wc(id: &WcId) -> AppError {
    let mut e =
        AppError::internal("This working copy is not in the list.").with_detail(id.0.clone());
    e.code = ErrorCode::NotAWorkingCopy;
    e
}

fn invalid_path(msg: &str) -> AppError {
    let mut e = AppError::internal(msg);
    e.code = ErrorCode::NotAWorkingCopy;
    e
}

impl WcRegistry {
    /// Registry over `db` that asks `engine` about working copies.
    pub fn new(db: AppDb, engine: Arc<dyn EngineFacade>) -> Self {
        Self::with_policy(db, engine, RetryPolicy::default())
    }

    /// Like [`WcRegistry::new`] with a custom read retry policy.
    pub fn with_policy(db: AppDb, engine: Arc<dyn EngineFacade>, policy: RetryPolicy) -> Self {
        Self {
            db,
            engine,
            policy,
            actors: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            open: Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// One actor per working copy root for the life of the process: removing and re-adding the same
    /// folder (new id) must not create a second writer on the same wc.db.
    fn actor_for(&self, root_path: &str) -> Arc<WcActor> {
        let mut actors = self.actors.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            actors
                .entry(root_path.to_owned())
                .or_insert_with(|| Arc::new(WcActor::spawn(self.policy))),
        )
    }

    fn live(&self, id: &str) -> Live {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Revisions known to be waiting on the server for a working copy.
    pub fn incoming_of(&self, id: &WcId) -> u32 {
        self.live(&id.0).incoming
    }

    /// Records the result of a remote check (incoming revisions and when it ran).
    pub fn set_incoming(&self, id: &WcId, count: u32) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = live.entry(id.0.clone()).or_default();
        entry.incoming = count;
        entry.last_checked_at = Some(now_iso());
    }

    /// Resolves a registry id to the engine reference and actor.
    pub fn resolve(&self, id: &WcId) -> Result<ResolvedWc, AppError> {
        let row = self.db.wc_get(&id.0)?.ok_or_else(|| unknown_wc(id))?;
        Ok(ResolvedWc {
            wc: WcRef {
                id: id.clone(),
                root: PathBuf::from(&row.root_path),
            },
            actor: self.actor_for(&row.root_path),
            row,
        })
    }

    /// Branch of the working copy at `url`: the trunk/branches/tags layout around it, detected on
    /// the server once per URL. `None` for folder-style repositories, a layout turned off in the
    /// settings, or while the server cannot be reached.
    async fn branch_of(
        &self,
        id: &str,
        repo_uuid: &str,
        repo_root_url: &str,
        url: &str,
    ) -> Option<BranchRef> {
        if !crate::layout::may_be_in_layout(repo_root_url, url) {
            return None;
        }
        match self.db.layout_get(repo_uuid) {
            Ok(Some(Layout::None)) => return None,
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "could not read the repository layout"),
        }
        let cached = self.live(id).branch;
        if let Some(c) = cached.filter(|c| c.url == url)
            && c.failed_at.is_none_or(|t| t.elapsed() < BRANCH_RETRY)
        {
            return c.branch;
        }
        let lister = crate::layout::EngineDirs(self.engine.as_ref());
        let detected = tokio::time::timeout(
            BRANCH_TIMEOUT,
            crate::layout::detect(&lister, repo_root_url, url),
        )
        .await;
        let entry = match detected {
            Ok(Ok(d)) => CachedBranch {
                url: url.to_owned(),
                branch: d.current,
                failed_at: None,
            },
            failure => {
                let code = match failure {
                    Ok(Err(e)) => Some(e.code),
                    _ => None,
                };
                tracing::debug!(wc = %id, ?code, "branch detection failed");
                CachedBranch {
                    url: url.to_owned(),
                    branch: None,
                    failed_at: Some(Instant::now()),
                }
            }
        };
        let branch = entry.branch.clone();
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.to_owned())
            .or_default()
            .branch = Some(entry);
        branch
    }

    async fn describe(&self, row: &WcRow) -> Result<WorkingCopy, AppError> {
        let wc = WcRef {
            id: WcId(row.id.clone()),
            root: PathBuf::from(&row.root_path),
        };
        let engine = Arc::clone(&self.engine);
        let info: WcInfo = self
            .actor_for(&row.root_path)
            .read(|| {
                let (engine, root) = (Arc::clone(&engine), wc.root.clone());
                async move {
                    engine
                        .wc_info(&root, &OpContext::detached())
                        .await
                        .map_err(|e| AppError::from(&e))
                }
            })
            .await?;
        let branch = self
            .branch_of(&row.id, &info.repo_uuid, &info.repo_root_url, &info.url)
            .await;
        let live = self.live(&row.id);
        Ok(WorkingCopy {
            id: WcId(row.id.clone()),
            name: row.name.clone(),
            root_path: info.root_path,
            repo_root_url: info.repo_root_url,
            repo_uuid: info.repo_uuid,
            url: info.url,
            base_rev: info.base_rev,
            rev_range: info.rev_range,
            format: info.format,
            store_pristine: info.store_pristine,
            branch,
            path_in_repo: info.path_in_repo,
            incoming: live.incoming,
            last_checked_at: live.last_checked_at,
            incomplete: info.incomplete,
        })
    }

    /// All registered working copies. One that the engine can no longer read (folder moved or
    /// deleted) is skipped and logged instead of failing the whole list.
    pub async fn list(&self) -> Result<Vec<WorkingCopy>, AppError> {
        let mut out = Vec::new();
        for row in self.db.wc_list()? {
            match self.describe(&row).await {
                Ok(wc) => out.push(wc),
                Err(e) => tracing::warn!(wc = %row.id, code = ?e.code, "working copy not readable"),
            }
        }
        Ok(out)
    }

    /// Every registered working copy for the repository manager, including the ones that cannot be
    /// read any more (`missing`), in list order.
    pub async fn entries(&self) -> Result<Vec<WcEntry>, AppError> {
        let mut out = Vec::new();
        for row in self.db.wc_list()? {
            let (url, incomplete, problem) = match self.describe(&row).await {
                Ok(wc) => (Some(wc.url), wc.incomplete, None),
                Err(e) => (None, false, Some(e.message)),
            };
            out.push(WcEntry {
                id: WcId(row.id),
                name: row.name,
                root_path: row.root_path,
                repo_root_url: row.repo_root_url,
                repo_uuid: row.repo_uuid,
                url,
                added_at: row.added_at,
                last_opened_at: row.last_opened_at,
                missing: problem.is_some(),
                problem,
                incomplete,
            });
        }
        Ok(out)
    }

    /// Points a registered working copy at the folder it was moved to. The folder must be a working
    /// copy of the same repository (same UUID) and must not be registered under another entry.
    /// The repository root URL is taken from the located working copy, which may have been
    /// relocated (`svn relocate`) while it was missing.
    pub async fn locate(&self, id: &WcId, path: &Path) -> Result<WorkingCopy, AppError> {
        let row = self.db.wc_get(&id.0)?.ok_or_else(|| unknown_wc(id))?;
        let shown = path.to_string_lossy().into_owned();
        let info = self
            .engine
            .wc_info(path, &OpContext::detached())
            .await
            .map_err(|e| AppError::from(&e).with_path(shown.clone()))?;
        if info.repo_uuid != row.repo_uuid {
            return Err(crate::errors::invalid_argument(
                "This folder is a working copy of another repository.",
            )
            .with_path(shown));
        }
        if let Some(other) = self.db.wc_get_by_root(&info.root_path)?
            && other.id != row.id
        {
            return Err(crate::errors::invalid_argument(format!(
                "This folder is already in the list as \"{}\".",
                other.name
            ))
            .with_path(shown));
        }
        self.db
            .wc_set_root(&row.id, &info.root_path, &info.repo_root_url)?;
        let row = self.db.wc_get(&id.0)?.ok_or_else(|| unknown_wc(id))?;
        self.describe(&row).await
    }

    /// Registers the working copy at `path` (idempotent: an already registered root is returned).
    pub async fn add_existing(&self, path: &str) -> Result<WorkingCopy, AppError> {
        if path.trim().is_empty() || path.contains('\0') || !Path::new(path).is_absolute() {
            return Err(invalid_path("The path must be an absolute folder path."));
        }
        let info = self
            .engine
            .wc_info(Path::new(path), &OpContext::detached())
            .await
            .map_err(|e| AppError::from(&e).with_path(path))?;
        let row = match self.db.wc_get_by_root(&info.root_path)? {
            Some(existing) => existing,
            None => {
                let name = Path::new(&info.root_path).file_name().map_or_else(
                    || info.root_path.clone(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let row = WcRow {
                    id: new_op_id().0, // UUID v7, same generator as operations
                    name,
                    root_path: info.root_path.clone(),
                    repo_uuid: info.repo_uuid.clone(),
                    repo_root_url: info.repo_root_url.clone(),
                    sort_order: 0,
                    added_at: String::new(),
                    last_opened_at: None,
                };
                self.db.wc_insert(&row)?;
                row
            }
        };
        self.describe(&row).await
    }

    /// After a checkout into `root` that did not finish (cancelled, connection lost): registers the
    /// partial working copy so the user can finish it with an update instead of being left with a
    /// folder that is neither in the list nor usable for a new checkout. Only a working copy whose
    /// root is exactly `root` (its administrative folder is directly inside) is registered, never
    /// an enclosing one. Returns `None` when there is nothing to register.
    ///
    /// The folder reads run on the blocking pool, never on the async thread (the folder may be
    /// large or on a slow network share).
    pub async fn add_unfinished(&self, root: &Path) -> Option<WorkingCopy> {
        let dir = root.to_path_buf();
        let has_admin_dir = tokio::task::spawn_blocking(move || {
            std::fs::read_dir(&dir).is_ok_and(|entries| {
                entries.flatten().any(|e| {
                    e.file_type().is_ok_and(|t| t.is_dir())
                        && svn_types::is_any_admin_dir_name(&e.file_name().to_string_lossy())
                })
            })
        })
        .await
        .unwrap_or(false);
        if !has_admin_dir {
            return None;
        }
        let info = self
            .engine
            .wc_info(root, &OpContext::detached())
            .await
            .ok()?;
        let (found, asked) = (PathBuf::from(&info.root_path), root.to_path_buf());
        let same_root = tokio::task::spawn_blocking(move || {
            let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            canonical(&found) == canonical(&asked)
        })
        .await
        .unwrap_or(false);
        if !same_root {
            return None;
        }
        match self.add_existing(&info.root_path).await {
            Ok(wc) => Some(wc),
            Err(e) => {
                tracing::warn!(code = ?e.code, "unfinished checkout was not registered");
                None
            }
        }
    }

    /// Removes the working copy from the list; its files stay on disk.
    pub fn remove(&self, id: &WcId) -> Result<(), AppError> {
        if !self.db.wc_remove(&id.0)? {
            return Err(unknown_wc(id));
        }
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id.0);
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id.0);
        Ok(())
    }

    /// Renames the list entry.
    pub fn rename(&self, id: &WcId, name: &str) -> Result<(), AppError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
            return Err(crate::errors::invalid_argument(
                "The name must be 1 to 200 characters.",
            ));
        }
        if self.db.wc_rename(&id.0, name)? {
            Ok(())
        } else {
            Err(unknown_wc(id))
        }
    }

    /// Opens the working copy through its actor: runs the engine's lock/crash recovery (a write),
    /// stamps the open time and returns the description with the recovery report.
    pub async fn open(&self, id: &WcId) -> Result<(WorkingCopy, RecoveryReport), AppError> {
        let row = self.db.wc_get(&id.0)?.ok_or_else(|| unknown_wc(id))?;
        let (engine, root) = (Arc::clone(&self.engine), PathBuf::from(&row.root_path));
        let opened = self
            .actor_for(&row.root_path)
            .write(async move {
                engine
                    .open_wc(&root, &OpContext::detached())
                    .await
                    .map_err(|e| AppError::from(&e))
            })
            .await?;
        self.db.wc_touch_opened(&id.0)?;
        Ok((self.describe(&row).await?, opened.recovery))
    }

    /// Marks a working copy as shown in the UI: it is watched and polled from now on.
    pub fn mark_open(&self, id: &WcId) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.0.clone());
    }

    /// Ids of the working copies the user has open (watched and polled).
    pub fn open_ids(&self) -> Vec<WcId> {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|id| WcId(id.clone()))
            .collect()
    }

    /// Ids of every registered working copy (used to run recovery at app start).
    pub fn ids(&self) -> Result<Vec<WcId>, AppError> {
        Ok(self.db.wc_list()?.into_iter().map(|r| WcId(r.id)).collect())
    }

    /// Releases per-working-copy resources. The actor finishes queued writes first.
    pub fn close(&self, id: &WcId) -> Result<(), AppError> {
        self.db.wc_get(&id.0)?.ok_or_else(|| unknown_wc(id))?;
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id.0);
        Ok(())
    }
}
