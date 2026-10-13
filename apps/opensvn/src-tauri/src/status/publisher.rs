//! Turns engine status into `wc://status-changed` deltas and keeps the snapshot cache current.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use opensvn_ipc::{AppError, FileStatus, Incoming, Rev, RevisionChanged, StatusChanged, WcId};

use crate::db::CacheDb;
use crate::engine::{EngineFacade, OpContext, RemoteScan, StatusQuery, WcInfo};
use crate::ops::AppEvents;
use crate::wc::ResolvedWc;

/// Status used for the snapshot: everything the Changes tab lists.
pub const SNAPSHOT_QUERY: StatusQuery = StatusQuery {
    include_ignored: false,
    include_unversioned: true,
};

type RevState = (Rev, Option<(Rev, Rev)>);

/// Publishes status deltas. Shared by explicit refreshes, finished write operations and (later)
/// the file system watcher.
pub struct StatusPublisher {
    engine: Arc<dyn EngineFacade>,
    cache: CacheDb,
    events: Arc<dyn AppEvents>,
    last_rev: Mutex<HashMap<String, RevState>>,
}

fn by_path(list: Vec<FileStatus>) -> BTreeMap<String, FileStatus> {
    list.into_iter().map(|s| (s.path.clone(), s)).collect()
}

impl StatusPublisher {
    /// Publisher over the engine and snapshot cache.
    pub fn new(engine: Arc<dyn EngineFacade>, cache: CacheDb, events: Arc<dyn AppEvents>) -> Self {
        Self {
            engine,
            cache,
            events,
            last_rev: Mutex::new(HashMap::new()),
        }
    }

    /// Last published snapshot (possibly stale), `None` when the working copy was never scanned.
    pub fn cached(&self, wc_id: &WcId) -> Result<Option<Vec<FileStatus>>, AppError> {
        Ok(self.cache.status_snapshot_get(wc_id)?)
    }

    /// Drops everything remembered about a working copy.
    pub fn forget(&self, wc_id: &WcId) {
        if let Err(e) = self.cache.status_snapshot_clear(wc_id) {
            tracing::warn!(error = %e, "could not clear status snapshot");
        }
        self.last_rev
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&wc_id.0);
    }

    /// Rescans the working copy, emits the delta against the snapshot (and a revision change when
    /// the base revision moved) and stores the new snapshot.
    pub async fn publish(&self, r: &ResolvedWc) -> Result<(), AppError> {
        let engine = Arc::clone(&self.engine);
        let wc = r.wc.clone();
        let (fresh, info) = r
            .actor
            .read(|| {
                let (engine, wc) = (Arc::clone(&engine), wc.clone());
                async move {
                    let ctx = OpContext::detached();
                    let statuses = engine
                        .status(&wc, SNAPSHOT_QUERY, &ctx)
                        .await
                        .map_err(|e| AppError::from(&e))?;
                    let info = engine
                        .wc_info(&wc.root, &ctx)
                        .await
                        .map_err(|e| AppError::from(&e))?;
                    Ok((statuses, info))
                }
            })
            .await?;
        self.apply_local(&r.wc.id, fresh)?;
        self.publish_revision(&r.wc.id, &info);
        Ok(())
    }

    fn apply_local(&self, wc_id: &WcId, fresh: Vec<FileStatus>) -> Result<(), AppError> {
        let old = by_path(self.cache.status_snapshot_get(wc_id)?.unwrap_or_default());
        let mut new = by_path(fresh);
        // Remote information comes from a different (network) check; keep it until that check
        // runs again.
        for (path, s) in &mut new {
            if let Some(prev) = old.get(path) {
                s.remote = prev.remote.clone();
            }
        }
        let changed: Vec<FileStatus> = new
            .iter()
            .filter(|(p, s)| old.get(*p) != Some(*s))
            .map(|(_, s)| s.clone())
            .collect();
        let removed: Vec<String> = old
            .keys()
            .filter(|p| !new.contains_key(*p))
            .cloned()
            .collect();
        self.cache
            .status_snapshot_replace(wc_id, &new.into_values().collect::<Vec<_>>())?;
        if !changed.is_empty() || !removed.is_empty() {
            self.events.status_changed(&StatusChanged {
                wc_id: wc_id.clone(),
                changed,
                removed,
            });
        }
        Ok(())
    }

    fn publish_revision(&self, wc_id: &WcId, info: &WcInfo) {
        let state: RevState = (info.base_rev, info.rev_range);
        let moved = {
            let mut map = self.last_rev.lock().unwrap_or_else(PoisonError::into_inner);
            map.insert(wc_id.0.clone(), state) != Some(state)
        };
        if moved {
            self.events.revision_changed(&RevisionChanged {
                wc_id: wc_id.clone(),
                base_rev: info.base_rev,
                rev_range: info.rev_range,
            });
        }
    }

    /// Merges the result of a remote check into the snapshot and announces it.
    pub fn apply_remote(&self, wc_id: &WcId, scan: &RemoteScan) -> Result<(), AppError> {
        let mut snapshot = by_path(self.cache.status_snapshot_get(wc_id)?.unwrap_or_default());
        // Nodes that were out of date before but are not any more lose their remote info.
        let mut changed = Vec::new();
        for s in snapshot.values_mut() {
            if s.remote.is_some() && !scan.statuses.iter().any(|n| n.path == s.path) {
                s.remote = None;
                changed.push(s.clone());
            }
        }
        for s in &scan.statuses {
            if snapshot.get(&s.path) != Some(s) {
                snapshot.insert(s.path.clone(), s.clone());
                changed.push(s.clone());
            }
        }
        self.cache
            .status_snapshot_replace(wc_id, &snapshot.into_values().collect::<Vec<_>>())?;
        if !changed.is_empty() {
            self.events.status_changed(&StatusChanged {
                wc_id: wc_id.clone(),
                changed,
                removed: Vec::new(),
            });
        }
        self.events.incoming(&Incoming {
            wc_id: wc_id.clone(),
            count: scan.incoming,
            latest_rev: scan.latest_rev,
        });
        Ok(())
    }
}
