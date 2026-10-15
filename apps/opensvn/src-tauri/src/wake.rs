//! Detects that the system woke up from sleep.
//!
//! No platform event is needed: a task ticks every few seconds and compares how much time passed
//! on the wall clock and on the monotonic clock. On Linux and macOS the monotonic clock stops
//! while the system sleeps, so the wall clock jumps ahead of it; where it keeps running
//! (Windows), the tick itself arrives far too late. Either way the connections opened before
//! the sleep are most likely dead: reads waiting on them are failed at once and operations that
//! wait for a lost connection try again (see `AppState::start_wake_watch`). A clock change by
//! the user or NTP may look like a wake; that only costs a reconnect.

use std::time::{Duration, Instant, SystemTime};

/// How often the watcher ticks.
pub const TICK: Duration = Duration::from_secs(5);
/// A gap beyond the tick that counts as a sleep.
const SLACK: Duration = Duration::from_secs(20);

/// Compares consecutive ticks.
#[derive(Debug)]
pub struct WakeDetector {
    last_mono: Instant,
    last_wall: SystemTime,
}

impl WakeDetector {
    /// Starts at `mono` / `wall`.
    pub fn new(mono: Instant, wall: SystemTime) -> Self {
        Self {
            last_mono: mono,
            last_wall: wall,
        }
    }

    /// Records a tick; true when the time since the previous one says the system slept.
    pub fn tick(&mut self, mono: Instant, wall: SystemTime) -> bool {
        let mono_gap = mono.saturating_duration_since(self.last_mono);
        let wall_gap = wall.duration_since(self.last_wall).unwrap_or_default();
        self.last_mono = mono;
        self.last_wall = wall;
        let late = mono_gap > TICK + SLACK;
        let suspended = wall_gap > mono_gap + SLACK;
        late || suspended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_ticks_are_not_a_wake() {
        let (m, w) = (Instant::now(), SystemTime::now());
        let mut d = WakeDetector::new(m, w);
        for i in 1..10u32 {
            assert!(!d.tick(m + TICK * i, w + TICK * i));
        }
    }

    #[test]
    fn a_wall_clock_jump_over_a_stopped_monotonic_clock_is_a_wake() {
        let (m, w) = (Instant::now(), SystemTime::now());
        let mut d = WakeDetector::new(m, w);
        // Linux, macOS: the monotonic clock did not run during 10 minutes of sleep.
        assert!(d.tick(m + TICK, w + TICK + Duration::from_secs(600)));
        assert!(!d.tick(m + TICK * 2, w + TICK * 2 + Duration::from_secs(600)));
    }

    #[test]
    fn a_tick_that_arrives_far_too_late_is_a_wake() {
        let (m, w) = (Instant::now(), SystemTime::now());
        let mut d = WakeDetector::new(m, w);
        // Windows: both clocks ran on, the timer fired only after the wake.
        let gap = Duration::from_secs(600);
        assert!(d.tick(m + gap, w + gap));
    }

    #[test]
    fn a_wall_clock_set_back_is_not_a_wake() {
        let (m, w) = (
            Instant::now(),
            SystemTime::now() + Duration::from_secs(3600),
        );
        let mut d = WakeDetector::new(m, w);
        assert!(!d.tick(m + TICK, w - Duration::from_secs(3000)));
    }
}
