//! Coalesces an operation's progress samples into at most one event per throttle interval.
//!
//! A large checkout reports one sample per path (100 000 and more). Sending each one would flood
//! the WebView, and dropping the ones inside the interval would lose paths from the log. The
//! coalescer instead merges every sample into the operation's current state, counts each path
//! under its action, and queues the paths for the next event. When the queue grows past
//! [`ENTRY_BATCH_LIMIT`] between two events, the oldest queued paths are dropped and counted in
//! `entriesDropped`: the UI only shows the most recent paths anyway, and the counts stay exact.

use std::collections::VecDeque;
use std::time::Instant;

use opensvn_ipc::{ActionCounts, ConnectionWait, OpId, OpStage, Progress, ProgressEntry, Size};

use super::throttle::Throttle;
use crate::engine::ProgressUpdate;

/// Paths kept per event; older paths queued since the previous event are dropped.
pub const ENTRY_BATCH_LIMIT: usize = 200;

/// Merged progress state of one operation.
#[derive(Debug)]
pub struct Coalescer {
    op_id: OpId,
    throttle: Throttle,
    phase: String,
    current: Option<u64>,
    total: Option<u64>,
    bytes_done: Option<u64>,
    /// The engine reported `bytes_done` itself (a download): the transport meter is ignored.
    explicit_bytes: bool,
    bytes_total: Option<u64>,
    current_path: Option<String>,
    stage: Option<OpStage>,
    connection_wait: Option<ConnectionWait>,
    counts: Option<ActionCounts>,
    entries: VecDeque<ProgressEntry>,
    dropped: u32,
    /// Something changed since the last event.
    dirty: bool,
}

impl Coalescer {
    /// Empty state for `op_id`, sending at most one event per `throttle` interval.
    pub fn new(op_id: OpId, throttle: Throttle) -> Self {
        Self {
            op_id,
            throttle,
            phase: String::new(),
            current: None,
            total: None,
            bytes_done: None,
            explicit_bytes: false,
            bytes_total: None,
            current_path: None,
            stage: None,
            connection_wait: None,
            counts: None,
            entries: VecDeque::new(),
            dropped: 0,
            dirty: false,
        }
    }

    /// Merges `u` into the state; returns the event to send now when the throttle allows one.
    pub fn push(&mut self, u: ProgressUpdate, now: Instant) -> Option<Progress> {
        if !u.phase.is_empty() {
            self.phase = u.phase;
        }
        if u.current.is_some() {
            self.current = u.current;
        }
        if u.total.is_some() {
            self.total = u.total;
        }
        if u.bytes_done.is_some() {
            self.bytes_done = u.bytes_done;
            self.explicit_bytes = true;
        }
        if u.bytes_total.is_some() {
            self.bytes_total = u.bytes_total;
        }
        if u.current_path.is_some() {
            self.current_path = u.current_path;
        }
        if u.stage.is_some() {
            self.stage = u.stage;
            // The wait timing belongs to its stage: any other stage clears it.
            self.connection_wait = u
                .connection_wait
                .filter(|_| self.stage == Some(OpStage::WaitingForConnection));
        }
        if let Some(entry) = u.entry {
            self.counts
                .get_or_insert_with(ActionCounts::default)
                .add(entry.action);
            if self.entries.len() == ENTRY_BATCH_LIMIT {
                self.entries.pop_front();
                self.dropped = self.dropped.saturating_add(1);
            }
            self.entries.push_back(entry);
        }
        self.dirty = true;
        self.poll(now)
    }

    /// Records the bytes the transport received so far (ignored once the engine reports bytes).
    pub fn set_received(&mut self, bytes: u64) {
        if self.explicit_bytes || bytes == 0 || self.bytes_done == Some(bytes) {
            return;
        }
        self.bytes_done = Some(bytes);
        self.dirty = true;
    }

    /// The pending event, when there is one and the throttle allows it.
    pub fn poll(&mut self, now: Instant) -> Option<Progress> {
        if self.dirty && self.throttle.allow(now) {
            Some(self.take())
        } else {
            None
        }
    }

    /// The pending event regardless of the throttle (the operation ends).
    pub fn flush(&mut self) -> Option<Progress> {
        self.dirty.then(|| self.take())
    }

    fn take(&mut self) -> Progress {
        self.dirty = false;
        Progress {
            op_id: self.op_id.clone(),
            phase: self.phase.clone(),
            current: self.current.map(Size),
            total: self.total.map(Size),
            bytes_done: self.bytes_done.map(Size),
            bytes_total: self.bytes_total.map(Size),
            current_path: self.current_path.clone(),
            stage: self.stage,
            entries: self.entries.drain(..).collect(),
            entries_dropped: std::mem::take(&mut self.dropped),
            counts: self.counts,
            connection_wait: self.connection_wait,
        }
    }
}

/// A notice sample (never coalesced): sent on its own, with nothing of the merged state.
pub fn notice(op_id: &OpId, u: ProgressUpdate) -> Progress {
    Progress {
        op_id: op_id.clone(),
        phase: u.phase,
        current: u.current.map(Size),
        total: u.total.map(Size),
        bytes_done: u.bytes_done.map(Size),
        bytes_total: u.bytes_total.map(Size),
        current_path: u.current_path,
        stage: u.stage,
        entries: Vec::new(),
        entries_dropped: 0,
        counts: None,
        connection_wait: u.connection_wait,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use opensvn_ipc::PathAction;

    use super::*;

    fn path_sample(i: u64, action: PathAction) -> ProgressUpdate {
        ProgressUpdate {
            phase: "checkout".into(),
            current: Some(i),
            current_path: Some(format!("f{i}")),
            entry: Some(ProgressEntry {
                action,
                path: format!("f{i}"),
            }),
            ..ProgressUpdate::default()
        }
    }

    fn coalescer() -> Coalescer {
        Coalescer::new(OpId("op".into()), Throttle::new())
    }

    #[test]
    fn a_hundred_thousand_paths_in_one_second_make_at_most_twenty_events() {
        let start = Instant::now();
        let mut c = coalescer();
        let mut sent = Vec::new();
        for i in 1..=100_000u64 {
            // 100 000 samples spread evenly over one second (one per 10 µs).
            let now = start + Duration::from_micros(i * 10);
            let action = if i % 10 == 0 {
                PathAction::Updated
            } else {
                PathAction::Added
            };
            sent.extend(c.push(path_sample(i, action), now));
        }
        sent.extend(c.flush());
        assert!(sent.len() <= 21, "sent {} events", sent.len());
        assert!(sent.len() >= 19, "sent {} events", sent.len());
        let last = sent.last().unwrap();
        assert_eq!(last.current.map(|s| s.0), Some(100_000));
        assert_eq!(last.current_path.as_deref(), Some("f100000"));
        let counts = last.counts.unwrap();
        assert_eq!(counts.added, 90_000);
        assert_eq!(counts.updated, 10_000);
        // Every path is either in a batch or counted as dropped.
        let carried: usize = sent.iter().map(|p| p.entries.len()).sum();
        let dropped: u32 = sent.iter().map(|p| p.entries_dropped).sum();
        assert_eq!(carried + dropped as usize, 100_000);
        assert!(sent.iter().all(|p| p.entries.len() <= ENTRY_BATCH_LIMIT));
        // The batch keeps the newest paths, in order.
        assert_eq!(last.entries.last().unwrap().path, "f100000");
    }

    #[test]
    fn paths_inside_the_interval_wait_for_the_next_event() {
        let now = Instant::now();
        let mut c = coalescer();
        let first = c.push(path_sample(1, PathAction::Added), now).unwrap();
        assert_eq!(first.entries.len(), 1);
        assert!(c.push(path_sample(2, PathAction::Added), now).is_none());
        assert!(c.push(path_sample(3, PathAction::Deleted), now).is_none());
        assert!(c.poll(now + Duration::from_millis(10)).is_none());
        let next = c.poll(now + Duration::from_millis(50)).unwrap();
        let paths: Vec<_> = next.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["f2", "f3"]);
        assert_eq!(next.entries_dropped, 0);
        assert_eq!(next.counts.unwrap().deleted, 1);
        // Nothing new: no event, even after the interval.
        assert!(c.poll(now + Duration::from_secs(1)).is_none());
        assert!(c.flush().is_none());
    }

    #[test]
    fn unchanged_fields_keep_their_last_value() {
        let now = Instant::now();
        let mut c = coalescer();
        c.push(ProgressUpdate::stage("checkout", OpStage::Fetching), now);
        let p = c
            .push(
                path_sample(1, PathAction::Added),
                now + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(p.stage, Some(OpStage::Fetching));
        assert_eq!(p.phase, "checkout");
    }

    #[test]
    fn transport_bytes_show_until_the_engine_reports_its_own() {
        let now = Instant::now();
        let mut c = coalescer();
        c.set_received(0);
        assert!(c.flush().is_none(), "no bytes, no event");
        c.set_received(4096);
        assert_eq!(c.poll(now).unwrap().bytes_done.map(|s| s.0), Some(4096));
        c.set_received(4096);
        assert!(c.flush().is_none(), "same count, no event");
        c.push(
            ProgressUpdate {
                phase: "network".into(),
                bytes_done: Some(10),
                ..ProgressUpdate::default()
            },
            now,
        );
        c.set_received(9999);
        assert_eq!(c.flush().unwrap().bytes_done.map(|s| s.0), Some(10));
    }
}
