//! Registry of working copies: persisted list plus one [`WcActor`] per working copy.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use opensvn_ipc::{AppError, ErrorCode, WcId, WorkingCopy};

use super::actor::{RetryPolicy, WcActor};
use crate::db::{AppDb, WcRow, now_iso};
use crate::engine::{EngineFacade, OpContext, RecoveryReport, WcInfo, WcRef};
use crate::ops::new_op_id;

/// Runtime facts about a working copy that are not persisted.
#[derive(Default, Clone)]
struct Live {
    incoming: u32,
    last_checked_at: Option<String>,
}

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
            branch: None,
            path_in_repo: info.path_in_repo,
            incoming: live.incoming,
            last_checked_at: live.last_checked_at,
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
                };
                self.db.wc_insert(&row)?;
                row
            }
        };
        self.describe(&row).await
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
