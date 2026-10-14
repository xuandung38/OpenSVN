//! Rate limit for progress events: at most one per interval per operation (see `coalesce`).

use std::time::{Duration, Instant};

/// Minimum spacing of progress events (20 per second).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Decides whether a sample taken at `now` may be sent. The first sample always passes; samples
/// inside the interval are dropped (a later sample carries the newer state anyway).
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    /// Throttle with the standard progress interval.
    pub fn new() -> Self {
        Self::with_interval(PROGRESS_INTERVAL)
    }

    /// Throttle with a custom interval.
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
        }
    }

    /// True when a sample at `now` should be delivered; records it as the last delivery.
    pub fn allow(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn at_most_twenty_per_second() {
        let start = Instant::now();
        let mut t = Throttle::new();
        // 1000 samples spread over one second (one per ms).
        let sent = (0..1000u64)
            .filter(|ms| t.allow(start + Duration::from_millis(*ms)))
            .count();
        assert!((19..=20).contains(&sent), "sent {sent}");
    }

    #[test]
    fn first_sample_passes_and_burst_is_dropped() {
        let now = Instant::now();
        let mut t = Throttle::new();
        assert!(t.allow(now));
        assert!(!t.allow(now));
        assert!(!t.allow(now + Duration::from_millis(49)));
        assert!(t.allow(now + Duration::from_millis(50)));
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let now = Instant::now();
        let mut t = Throttle::new();
        assert!(t.allow(now + Duration::from_secs(1)));
        assert!(!t.allow(now));
    }
}
