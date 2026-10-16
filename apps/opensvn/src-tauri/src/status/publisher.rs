//! Turns engine status into `wc://status-changed` deltas and keeps the snapshot cache current.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use opensvn_ipc::{
    AppError, FileStatus, Incoming, Rev, RevisionChanged, StatusChanged, StatusSnapshot, WcId,
};

use crate::db::CacheDb;
use crate::engine::{EngineFacade, OpContext, RemoteScan, StatusQuery, WcInfo};
use crate::ops::AppEvents;
use crate::wc::ResolvedWc;

/// Status used for the snapshot: everything the Changes tab lists.
pub const SNAPSHOT_QUERY: StatusQuery = StatusQuery {
    include_ignored: false,
    include_unversioned: true,
};

/// What a `wc://revision-changed` announces: the revisions and the URL (a switch at the same
/// revision moves the working copy too, and the toolbar shows its location).
type RevState = (Rev, Option<(Rev, Rev)>, String);

/// Publishes status deltas. Shared by explicit refreshes, finished write operations and (later)
/// the file system watcher.
pub struct StatusPublisher {
    engine: Arc<dyn EngineFacade>,
    cache: CacheDb,
    events: Arc<dyn AppEvents>,
    last_rev: Mutex<HashMap<String, RevState>>,
    gens: Mutex<HashMap<String, Arc<Generation>>>,
    /// Id of the stored snapshot of each working copy, see [`SnapState`].
    snaps: Mutex<HashMap<String, SnapState>>,
    /// Last snapshot id handed out (ids are unique across working copies).
    last_id: AtomicU32,
}

/// What the stored snapshot of a working copy is to the UI.
#[derive(Debug, Clone, Copy, Default)]
struct SnapState {
    /// Id the snapshot was given (0: none yet). Deltas are announced against it.
    id: u32,
    /// A scan of this app run produced it (rather than an earlier run's cache).
    fresh: bool,
}

/// Orders publications of one working copy: a scan started earlier never overwrites the result of
/// a scan started later, and delta computation + snapshot replacement are one atomic step.
#[derive(Default)]
struct Generation {
    next: AtomicU64,
    applied: tokio::sync::Mutex<u64>,
    /// Scans running now.
    running: AtomicUsize,
    /// Signalled whenever a scan ends.
    ended: tokio::sync::Notify,
}

/// Counts a running scan for [`StatusPublisher::snapshot`] until dropped.
struct Running(Arc<Generation>);

impl Running {
    fn start(generation: &Arc<Generation>) -> Self {
        generation.running.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(generation))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
        self.0.ended.notify_waiters();
    }
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
            gens: Mutex::new(HashMap::new()),
            snaps: Mutex::new(HashMap::new()),
            last_id: AtomicU32::new(0),
        }
    }

    fn snaps(&self) -> std::sync::MutexGuard<'_, HashMap<String, SnapState>> {
        self.snaps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn new_id(&self) -> u32 {
        self.last_id
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1)
            .max(1)
    }

    /// The snapshot of a working copy with its id, for `status_get`: the stored one when a scan
    /// of this app run made it (the watcher keeps it current) or while a write runs (the running
    /// operation is the "updating" indicator). Otherwise it waits for the scan already running,
    /// or scans.
    pub async fn snapshot(&self, r: &ResolvedWc) -> Result<StatusSnapshot, AppError> {
        let generation = self.generation(&r.wc.id);
        loop {
            {
                let _applying = generation.applied.lock().await;
                let state = self.snaps().get(&r.wc.id.0).copied().unwrap_or_default();
                if state.fresh || r.actor.is_writing() {
                    if let Some(files) = self.cache.status_snapshot_get(&r.wc.id)? {
                        let id = self.hand_out(&r.wc.id);
                        return Ok(StatusSnapshot {
                            snapshot: Some(id),
                            files,
                        });
                    }
                }
                if generation.running.load(Ordering::SeqCst) > 0 {
                    // Registered before the lock is released, so the end of that scan is seen.
                    let ended = generation.ended.notified();
                    drop(_applying);
                    ended.await;
                    continue;
                }
            }
            self.publish(r).await?;
            if !self.snaps().get(&r.wc.id.0).is_some_and(|s| s.fresh) {
                // The scan did not store anything (a later one won the race and failed): return
                // what is stored, or nothing.
                let files = self
                    .cache
                    .status_snapshot_get(&r.wc.id)?
                    .unwrap_or_default();
                let id = self.hand_out(&r.wc.id);
                return Ok(StatusSnapshot {
                    snapshot: Some(id),
                    files,
                });
            }
        }
    }

    /// Id of the stored snapshot, given one if it has none yet. Called under `applied`.
    fn hand_out(&self, wc_id: &WcId) -> u32 {
        let mut snaps = self.snaps();
        let state = snaps.entry(wc_id.0.clone()).or_default();
        if state.id == 0 {
            state.id = self.new_id();
        }
        state.id
    }

    /// Records that the stored snapshot changed (called under `applied`): the delta's base and
    /// the new id.
    fn advance(&self, wc_id: &WcId, fresh: bool) -> (Option<u32>, u32) {
        let id = self.new_id();
        let mut snaps = self.snaps();
        let state = snaps.entry(wc_id.0.clone()).or_default();
        let base = (state.id != 0).then_some(state.id);
        state.id = id;
        state.fresh |= fresh;
        (base, id)
    }

    /// Last published snapshot (possibly stale), `None` when the working copy was never scanned.
    pub fn cached(&self, wc_id: &WcId) -> Result<Option<Vec<FileStatus>>, AppError> {
        Ok(self.cache.status_snapshot_get(wc_id)?)
    }

    /// Drops the snapshots of every working copy. Nothing else is stored in the cache, so this is
    /// all "Clear cache" removes; remote check results return with the next check.
    pub fn clear_cache(&self) -> Result<(), AppError> {
        // Deltas against the dropped snapshots would be wrong: holders refetch.
        self.snaps().clear();
        Ok(self.cache.status_snapshot_clear_all()?)
    }

    /// Drops everything remembered about a working copy.
    pub fn forget(&self, wc_id: &WcId) {
        if let Err(e) = self.cache.status_snapshot_clear(wc_id) {
            tracing::warn!(error = %e, "could not clear status snapshot");
        }
        self.snaps().remove(&wc_id.0);
        self.last_rev
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&wc_id.0);
    }

    /// Rescans the working copy, emits the delta against the snapshot (and a revision change when
    /// the base revision moved) and stores the new snapshot.
    pub async fn publish(&self, r: &ResolvedWc) -> Result<(), AppError> {
        let generation = self.generation(&r.wc.id);
        let _running = Running::start(&generation);
        // The ticket is taken before the scan starts, so it orders scans by start time.
        let ticket = generation.next.fetch_add(1, Ordering::SeqCst) + 1;
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
        let mut applied = generation.applied.lock().await;
        if ticket < *applied {
            return Ok(()); // a scan that started later already published newer state
        }
        *applied = ticket;
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
        let unchanged = changed.is_empty() && removed.is_empty();
        let stored = self.snaps().get(&wc_id.0).copied().unwrap_or_default();
        if unchanged && stored.fresh {
            return Ok(());
        }
        self.cache
            .status_snapshot_replace(wc_id, &new.into_values().collect::<Vec<_>>())?;
        if unchanged {
            // Same list as the earlier run's cache, now known current: the id stays.
            self.snaps().entry(wc_id.0.clone()).or_default().fresh = true;
            return Ok(());
        }
        let (base, snapshot) = self.advance(wc_id, true);
        self.events.status_changed(&StatusChanged {
            wc_id: wc_id.clone(),
            changed,
            removed,
            base,
            snapshot,
        });
        Ok(())
    }

    fn publish_revision(&self, wc_id: &WcId, info: &WcInfo) {
        let state: RevState = (info.base_rev, info.rev_range, info.url.clone());
        let moved = {
            let mut map = self.last_rev.lock().unwrap_or_else(PoisonError::into_inner);
            map.get(&wc_id.0) != Some(&state) && {
                map.insert(wc_id.0.clone(), state);
                true
            }
        };
        if moved {
            self.events.revision_changed(&RevisionChanged {
                wc_id: wc_id.clone(),
                base_rev: info.base_rev,
                rev_range: info.rev_range,
            });
        }
    }

    fn generation(&self, wc_id: &WcId) -> Arc<Generation> {
        let mut gens = self.gens.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(gens.entry(wc_id.0.clone()).or_default())
    }

    /// Merges the result of a remote check into the snapshot and announces it.
    pub async fn apply_remote(&self, wc_id: &WcId, scan: &RemoteScan) -> Result<(), AppError> {
        let generation = self.generation(wc_id);
        let _applying = generation.applied.lock().await;
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
        if !changed.is_empty() {
            self.cache
                .status_snapshot_replace(wc_id, &snapshot.into_values().collect::<Vec<_>>())?;
            let (base, snapshot) = self.advance(wc_id, false);
            self.events.status_changed(&StatusChanged {
                wc_id: wc_id.clone(),
                changed,
                removed: Vec::new(),
                base,
                snapshot,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db::CacheDb;
    use crate::engine::fake::demo_wc_root;
    use crate::engine::{FakeEngine, RemoteScan, WcRef};
    use crate::ops::events::{Recorded, RecordingEvents};
    use crate::wc::WcActor;
    use std::time::Duration;

    #[tokio::test]
    async fn overlapping_publishes_keep_the_snapshot_and_events_consistent() {
        let engine = Arc::new(FakeEngine::demo(Duration::ZERO).await.unwrap());
        let events = Arc::new(RecordingEvents::default());
        let publisher = Arc::new(StatusPublisher::new(
            engine.clone(),
            CacheDb::open_in_memory().unwrap(),
            events.clone(),
        ));
        let resolved = ResolvedWc {
            wc: WcRef {
                id: WcId("w".into()),
                root: demo_wc_root(),
            },
            actor: Arc::new(WcActor::spawn(Default::default())),
            row: crate::db::WcRow {
                id: "w".into(),
                name: "demo".into(),
                root_path: demo_wc_root().to_string_lossy().into_owned(),
                repo_uuid: "u".into(),
                repo_root_url: "u".into(),
                sort_order: 0,
                added_at: String::new(),
                last_opened_at: None,
            },
        };
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (p, r) = (publisher.clone(), resolved.clone());
                tokio::spawn(async move { p.publish(&r).await })
            })
            .collect();
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        // Whatever the interleaving, the first delta announced everything and later ones nothing
        // new: replaying all deltas yields exactly the snapshot.
        let mut replay = std::collections::BTreeMap::new();
        for r in events.all() {
            if let Recorded::Status(s) = r {
                for c in s.changed {
                    replay.insert(c.path.clone(), c);
                }
                for gone in s.removed {
                    replay.remove(&gone);
                }
            }
        }
        let snapshot = by_path(publisher.cached(&WcId("w".into())).unwrap().unwrap());
        assert_eq!(replay, snapshot);
    }

    fn fixture() -> (Arc<StatusPublisher>, Arc<RecordingEvents>, ResolvedWc) {
        let events = Arc::new(RecordingEvents::default());
        let engine = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(FakeEngine::demo(Duration::ZERO))
                .unwrap()
        });
        let publisher = Arc::new(StatusPublisher::new(
            Arc::new(engine),
            CacheDb::open_in_memory().unwrap(),
            events.clone(),
        ));
        let resolved = ResolvedWc {
            wc: WcRef {
                id: WcId("w".into()),
                root: demo_wc_root(),
            },
            actor: Arc::new(WcActor::spawn(Default::default())),
            row: crate::db::WcRow {
                id: "w".into(),
                name: "demo".into(),
                root_path: demo_wc_root().to_string_lossy().into_owned(),
                repo_uuid: "u".into(),
                repo_root_url: "u".into(),
                sort_order: 0,
                added_at: String::new(),
                last_opened_at: None,
            },
        };
        (publisher, events, resolved)
    }

    fn deltas(events: &RecordingEvents) -> Vec<StatusChanged> {
        events
            .all()
            .into_iter()
            .filter_map(|r| match r {
                Recorded::Status(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    fn apply(files: &mut BTreeMap<String, FileStatus>, d: &StatusChanged) {
        for c in &d.changed {
            files.insert(c.path.clone(), c.clone());
        }
        for gone in &d.removed {
            files.remove(gone);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deltas_chain_from_the_snapshot_status_get_hands_out() {
        let (publisher, events, r) = fixture();
        let first = publisher.snapshot(&r).await.unwrap();
        let id = first.snapshot.unwrap();
        // The first scan's delta (against nothing handed out) came before the id was given.
        assert!(deltas(&events).iter().all(|d| d.snapshot <= id));
        // A fresh snapshot is served again without a change of id.
        assert_eq!(publisher.snapshot(&r).await.unwrap(), first);

        let mut remote = first.files[0].clone();
        remote.remote = Some(opensvn_ipc::RemoteStatus {
            out_of_date: true,
            remote_rev: Some(Rev(99)),
            remote_lock: None,
        });
        let scan = RemoteScan {
            statuses: vec![remote.clone()],
            incoming: 1,
            latest_rev: Rev(99),
        };
        publisher.apply_remote(&r.wc.id, &scan).await.unwrap();
        let delta = deltas(&events).pop().unwrap();
        assert_eq!(delta.base, Some(id));
        assert!(delta.snapshot > id);
        let mut held = by_path(first.files);
        apply(&mut held, &delta);
        let second = publisher.snapshot(&r).await.unwrap();
        assert_eq!(second.snapshot, Some(delta.snapshot));
        assert_eq!(held, by_path(second.files));

        // An unchanged rescan announces nothing and keeps the id.
        let before = deltas(&events).len();
        publisher.publish(&r).await.unwrap();
        assert_eq!(deltas(&events).len(), before);
        assert_eq!(
            publisher.snapshot(&r).await.unwrap().snapshot,
            Some(delta.snapshot)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn after_clear_cache_the_next_delta_has_no_base() {
        let (publisher, events, r) = fixture();
        publisher.snapshot(&r).await.unwrap();
        publisher.clear_cache().unwrap();
        publisher.publish(&r).await.unwrap();
        let delta = deltas(&events).pop().unwrap();
        assert_eq!(delta.base, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_get_during_scans_matches_the_deltas_that_follow() {
        let (publisher, events, r) = fixture();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (p, r) = (publisher.clone(), r.clone());
                tokio::spawn(async move { p.publish(&r).await })
            })
            .collect();
        let got = publisher.snapshot(&r).await.unwrap();
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        let id = got.snapshot.unwrap();
        let mut held = by_path(got.files);
        for d in deltas(&events) {
            if d.base == Some(id) || (d.base.is_some() && d.snapshot > id) {
                apply(&mut held, &d);
            }
        }
        let now = by_path(publisher.cached(&r.wc.id).unwrap().unwrap());
        assert_eq!(held, now);
    }
}
