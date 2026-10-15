//! Detects that the system woke up from sleep.
//!
//! No platform event is needed: a task ticks every few seconds and compares how much time passed
//! on two clocks that the user's clock settings never move. The monotonic clock ([`Instant`])
//! stops while the system sleeps on Linux and macOS; the boot clock ([`boot_clock`]:
//! `CLOCK_BOOTTIME` on Linux, `CLOCK_MONOTONIC` on macOS) keeps counting, so it runs ahead of
//! the monotonic one after a sleep. Where the monotonic clock itself keeps running (Windows),
//! the tick arrives far too late. Either way the connections opened before the sleep are most
//! likely dead: reads waiting on them are failed at once and operations that wait for a lost
//! connection try again (see `AppState::start_wake_watch`).
//!
//! The wall clock is never used: setting the clock, an NTP step, a time zone or DST change must
//! not look like a wake and break healthy reads.

use std::time::{Duration, Instant};

/// How often the watcher ticks.
pub const TICK: Duration = Duration::from_secs(5);
/// A gap beyond the tick that counts as a sleep.
const SLACK: Duration = Duration::from_secs(20);

/// Time since boot, sleep included, on a clock that setting the time does not move; `None`
/// where there is none (Windows, where [`Instant`] already counts the sleep).
pub fn boot_clock() -> Option<Duration> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let id = rustix::time::ClockId::Boottime;
    // On Apple systems `CLOCK_MONOTONIC` counts the sleep; `Instant` uses `CLOCK_UPTIME_RAW`,
    // which does not.
    #[cfg(target_vendor = "apple")]
    let id = rustix::time::ClockId::Monotonic;
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        let ts = rustix::time::clock_gettime(id);
        let secs = u64::try_from(ts.tv_sec).ok()?;
        let nanos = u32::try_from(ts.tv_nsec).ok()?;
        Some(Duration::new(secs, nanos))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        None
    }
}

/// Compares consecutive ticks.
#[derive(Debug)]
pub struct WakeDetector {
    last_mono: Instant,
    last_boot: Option<Duration>,
}

impl WakeDetector {
    /// Starts at `mono` / `boot` ([`boot_clock`]).
    pub fn new(mono: Instant, boot: Option<Duration>) -> Self {
        Self {
            last_mono: mono,
            last_boot: boot,
        }
    }

    /// Records a tick; true when the time since the previous one says the system slept.
    pub fn tick(&mut self, mono: Instant, boot: Option<Duration>) -> bool {
        let mono_gap = mono.saturating_duration_since(self.last_mono);
        let boot_gap = match (self.last_boot, boot) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => Duration::ZERO,
        };
        self.last_mono = mono;
        self.last_boot = boot;
        let late = mono_gap > TICK + SLACK;
        let suspended = boot_gap > mono_gap + SLACK;
        late || suspended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_ticks_are_not_a_wake() {
        let (m, b) = (Instant::now(), Duration::from_secs(1000));
        let mut d = WakeDetector::new(m, Some(b));
        for i in 1..10u32 {
            assert!(!d.tick(m + TICK * i, Some(b + TICK * i)));
        }
    }

    #[test]
    fn a_boot_clock_ahead_of_a_stopped_monotonic_clock_is_a_wake() {
        let (m, b) = (Instant::now(), Duration::from_secs(1000));
        let mut d = WakeDetector::new(m, Some(b));
        // Linux, macOS: the monotonic clock did not run during 10 minutes of sleep.
        let slept = Duration::from_secs(600);
        assert!(d.tick(m + TICK, Some(b + TICK + slept)));
        assert!(!d.tick(m + TICK * 2, Some(b + TICK * 2 + slept)));
    }

    #[test]
    fn a_tick_that_arrives_far_too_late_is_a_wake() {
        let m = Instant::now();
        let mut d = WakeDetector::new(m, None);
        // Windows: the monotonic clock ran on, the timer fired only after the wake.
        assert!(d.tick(m + Duration::from_secs(600), None));
        assert!(!d.tick(m + Duration::from_secs(605), None));
    }

    #[test]
    fn without_a_boot_clock_regular_ticks_are_not_a_wake() {
        let m = Instant::now();
        let mut d = WakeDetector::new(m, None);
        for i in 1..10u32 {
            assert!(!d.tick(m + TICK * i, None));
        }
    }

    #[test]
    fn the_real_clocks_do_not_report_a_wake_between_two_close_ticks() {
        let mut d = WakeDetector::new(Instant::now(), boot_clock());
        assert!(!d.tick(Instant::now(), boot_clock()));
    }

    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn the_boot_clock_runs_with_the_monotonic_clock_while_awake() {
        let (m0, b0) = (Instant::now(), boot_clock().unwrap_or_default());
        std::thread::sleep(Duration::from_millis(50));
        let (m1, b1) = (Instant::now(), boot_clock().unwrap_or_default());
        let (mono, boot) = (m1 - m0, b1.saturating_sub(b0));
        assert!(boot >= Duration::from_millis(40), "{boot:?}");
        assert!(
            boot.abs_diff(mono) < Duration::from_millis(20),
            "{boot:?} vs {mono:?}"
        );
    }
}
