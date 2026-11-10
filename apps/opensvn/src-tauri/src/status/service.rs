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

/// First wait before a rescan that failed is tried again; it doubles for each further failure.
pub const RETRY_BASE: Duration = Duration::from_secs(1);

/// Failed rescans in a row after which the watcher waits for the next file event.
const MAX_RETRIES: u32 = 5;

/// Timing knobs (tests shorten them).
#[derive(Debug, Clone, Copy)]
pub struct StatusTiming {
    /// Quiet period before a burst of events triggers a rescan.
    pub debounce: Duration,
    /// Rescan period in fallback mode.
    pub fallback_poll: Duration,
    /// First wait before a failed rescan is tried again (doubles each time).
    pub retry_base: Duration,
}

impl Default for StatusTiming {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            fallback_poll: FALLBACK_POLL,
            retry_base: RETRY_BASE,
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
    // A rescan that fails (the working copy is busy with a write, another program holds its lock)
    // is tried again: with a watch there is no periodic rescan, so the list would stay as it was
    // until the next file event.
    let mut failures: u32 = 0;
    let mut retry_at: Option<Instant> = None;
    loop {
        let deadline = debouncer.deadline().map(Instant::from_std);
        // Biased: pending file events are taken before the quiet-period deadline, so a burst that
        // is still arriving extends the debounce instead of triggering a scan in the middle of it.
        tokio::select! {
            biased;
            event = rx.recv(), if watching => {
                let Some((path, is_dir)) = event else { break };
                debouncer.push(&path, is_dir, std::time::Instant::now());
                // The scan this event leads to starts over; it schedules its own retry.
                failures = 0;
                retry_at = None;
            }
            () = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                if debouncer.take_ready(std::time::Instant::now()).is_some() {
                    rescan(&wc, &publisher, timing, &mut failures, &mut retry_at, "after file change").await;
                }
            }
            () = async {
                match retry_at {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => {
                retry_at = None;
                rescan(&wc, &publisher, timing, &mut failures, &mut retry_at, "retry").await;
            }
            _ = poll.tick(), if !watching => {
                if let Err(e) = publisher.publish(&wc).await {
                    tracing::warn!(code = ?e.code, "periodic status refresh failed");
                }
            }
        }
    }
}

/// Wait before the retry that follows the `failures`-th failed rescan in a row; `None` when the
/// tries are used up (or nothing failed).
fn retry_delay(base: Duration, failures: u32) -> Option<Duration> {
    if failures == 0 || failures > MAX_RETRIES {
        return None;
    }
    base.checked_mul(1 << (failures - 1))
}

/// Publishes the working copy status; on failure schedules the next try.
async fn rescan(
    wc: &ResolvedWc,
    publisher: &StatusPublisher,
    timing: StatusTiming,
    failures: &mut u32,
    retry_at: &mut Option<Instant>,
    why: &str,
) {
    match publisher.publish(wc).await {
        Ok(()) => {
            *failures = 0;
            *retry_at = None;
        }
        Err(e) => {
            *failures += 1;
            *retry_at = retry_delay(timing.retry_base, *failures).map(|d| Instant::now() + d);
            tracing::warn!(
                code = ?e.code,
                attempt = *failures,
                will_retry = retry_at.is_some(),
                "status refresh {why} failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_and_stop() {
        let base = Duration::from_secs(1);
        assert_eq!(retry_delay(base, 0), None);
        let waits: Vec<_> = (1..=MAX_RETRIES)
            .map(|n| retry_delay(base, n).map(|d| d.as_secs()))
            .collect();
        assert_eq!(waits, [Some(1), Some(2), Some(4), Some(8), Some(16)]);
        assert_eq!(retry_delay(base, MAX_RETRIES + 1), None);
    }
}
