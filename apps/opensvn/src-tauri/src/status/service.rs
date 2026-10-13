//! Keeps status current for open working copies: watches the file system, debounces events and
//! publishes deltas. When a watch cannot be created (inotify limit, network drive, missing
//! folder) it falls back to periodic rescans.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::debounce::{DEBOUNCE, Debouncer};
use super::publisher::StatusPublisher;
use super::watcher::{FsWatcher, WatchError};
use crate::wc::ResolvedWc;

/// Rescan period when no watch is available.
pub const FALLBACK_POLL: Duration = Duration::from_secs(30);

/// Timing knobs (tests shorten them).
#[derive(Debug, Clone, Copy)]
pub struct StatusTiming {
    /// Quiet period before a burst of events triggers a rescan.
    pub debounce: Duration,
    /// Rescan period in fallback mode.
    pub fallback_poll: Duration,
}

impl Default for StatusTiming {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            fallback_poll: FALLBACK_POLL,
        }
    }
}

/// Starts and stops per-working-copy watch tasks.
pub struct StatusService {
    publisher: Arc<StatusPublisher>,
    timing: StatusTiming,
    tasks: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl StatusService {
    /// Service publishing through `publisher`.
    pub fn new(publisher: Arc<StatusPublisher>, timing: StatusTiming) -> Self {
        Self {
            publisher,
            timing,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Starts watching a working copy (replacing a previous watch of the same one).
    pub fn start(&self, wc: ResolvedWc) {
        let id = wc.wc.id.0.clone();
        let publisher = Arc::clone(&self.publisher);
        let timing = self.timing;
        let task = tokio::spawn(async move { watch_loop(wc, publisher, timing).await });
        if let Some(old) = self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, task)
        {
            old.abort();
        }
    }

    /// Stops watching a working copy.
    pub fn stop(&self, id: &str) {
        if let Some(task) = self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
        {
            task.abort();
        }
    }

    /// Number of active watches.
    #[cfg(test)]
    pub fn active(&self) -> usize {
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

async fn watch_loop(wc: ResolvedWc, publisher: Arc<StatusPublisher>, timing: StatusTiming) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _watch = match FsWatcher::start(&wc.wc.root, tx) {
        Ok(w) => Some(w),
        Err(WatchError::LimitReached) => {
            tracing::warn!(
                wc = %wc.wc.id.0,
                "file watch limit reached (raise fs.inotify.max_user_watches); rescanning periodically"
            );
            None
        }
        Err(WatchError::Other(e)) => {
            tracing::warn!(wc = %wc.wc.id.0, error = %e, "cannot watch folder; rescanning periodically");
            None
        }
    };
    let watching = _watch.is_some();
    let mut debouncer = Debouncer::new(timing.debounce);
    let mut poll = tokio::time::interval(timing.fallback_poll);
    poll.tick().await; // the first tick is immediate; the open path already scanned
    loop {
        let deadline = debouncer.deadline().map(Instant::from_std);
        // Biased: pending file events are taken before the quiet-period deadline, so a burst that
        // is still arriving extends the debounce instead of triggering a scan in the middle of it.
        tokio::select! {
            biased;
            event = rx.recv(), if watching => {
                let Some((path, is_dir)) = event else { break };
                debouncer.push(&path, is_dir, std::time::Instant::now());
            }
            () = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                if debouncer.take_ready(std::time::Instant::now()).is_some()
                    && let Err(e) = publisher.publish(&wc).await
                {
                    tracing::warn!(code = ?e.code, "status refresh after file change failed");
                }
            }
            _ = poll.tick(), if !watching => {
                if let Err(e) = publisher.publish(&wc).await {
                    tracing::warn!(code = ?e.code, "periodic status refresh failed");
                }
            }
        }
    }
}
