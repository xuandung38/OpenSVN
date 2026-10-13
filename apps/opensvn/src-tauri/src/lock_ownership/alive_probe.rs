//! Real process liveness probe for working copy lock owners (macOS, Linux, Windows).
//!
//! An owner is a `(pid, boot_id)` pair. The boot id tells a still-running process from a pid that
//! was recycled after a reboot.

use std::sync::OnceLock;

use sysinfo::System;
#[cfg(not(target_os = "linux"))]
use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate};

/// Allowed difference between two derived boot times before they count as different boots.
/// Absorbs clock adjustments; a real reboot is always far apart.
const BOOT_TIME_TOLERANCE_SECS: u64 = 120;

const BOOT_TIME_PREFIX: &str = "boot-time:";

/// Identifier of the current boot session. Computed once per process so every record written by
/// this process carries the same value.
pub fn current_boot_id() -> &'static str {
    static BOOT_ID: OnceLock<String> = OnceLock::new();
    BOOT_ID.get_or_init(|| {
        #[cfg(target_os = "linux")]
        if let Ok(id) = std::fs::read_to_string("/proc/sys/kernel/random/boot_id") {
            let id = id.trim();
            if !id.is_empty() {
                return format!("linux:{id}");
            }
        }
        #[cfg(target_os = "macos")]
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
        {
            let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if out.status.success() && !id.is_empty() {
                return format!("macos:{id}");
            }
        }
        format!("{BOOT_TIME_PREFIX}{}", System::boot_time())
    })
}

/// Whether `recorded` and `current` identify the same boot session.
pub fn same_boot(recorded: &str, current: &str) -> bool {
    if recorded == current {
        return true;
    }
    match (
        recorded
            .strip_prefix(BOOT_TIME_PREFIX)
            .and_then(|s| s.parse::<u64>().ok()),
        current
            .strip_prefix(BOOT_TIME_PREFIX)
            .and_then(|s| s.parse::<u64>().ok()),
    ) {
        (Some(a), Some(b)) => a.abs_diff(b) <= BOOT_TIME_TOLERANCE_SECS,
        // Linux boot ids are exact; mixed or unknown kinds never match.
        _ => false,
    }
}

/// What the lock ownership store needs to know about processes.
pub trait ProcessProbe: Send + Sync {
    /// Opaque start stamp of the live, non-zombie process `pid`; `None` when no such process
    /// exists. Stamps are only comparable through [`ProcessProbe::same_process`], and only within
    /// one boot session.
    fn start_time(&self, pid: u32) -> Option<u64>;
    /// Whether two stamps of the same pid identify the same process.
    fn same_process(&self, recorded: u64, current: u64) -> bool;
    /// Whether `recorded` names the boot session the machine is in now.
    fn same_boot(&self, recorded: &str) -> bool;
}

/// Tolerance for stamps that are wall-clock seconds (macOS, Windows).
const WALL_CLOCK_TOLERANCE_SECS: u64 = 2;

/// Parses `/proc/<pid>/stat`: the process state and `starttime` (field 22, clock ticks since boot).
/// The command name (field 2) may contain spaces and parentheses, so parsing starts after the last
/// `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_proc_stat(content: &str) -> Option<(char, u64)> {
    let after = content.get(content.rfind(')')? + 1..)?;
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?; // field 3
    let start = fields.nth(18)?.parse().ok()?; // field 22
    Some((state, start))
}

/// Start stamp on Linux: ticks since boot from `/proc`. Unlike a wall-clock start time it cannot
/// move when the system clock is set or NTP steps it, so a live owner never looks dead because
/// of a clock jump.
#[cfg(target_os = "linux")]
fn linux_start_stamp(pid: u32) -> Option<u64> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, start) = parse_proc_stat(&content)?;
    (!matches!(state, 'Z' | 'X' | 'x')).then_some(start)
}

/// [`ProcessProbe`] backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsProcessProbe;

impl ProcessProbe for OsProcessProbe {
    fn start_time(&self, pid: u32) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            linux_start_stamp(pid)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let pid = Pid::from_u32(pid);
            let mut sys = System::new();
            sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
            sys.process(pid)
                .filter(|p| !matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead))
                .map(sysinfo::Process::start_time)
        }
    }

    fn same_process(&self, recorded: u64, current: u64) -> bool {
        if cfg!(target_os = "linux") {
            recorded == current // ticks since boot: exact
        } else {
            recorded.abs_diff(current) <= WALL_CLOCK_TOLERANCE_SECS
        }
    }

    fn same_boot(&self, recorded: &str) -> bool {
        same_boot(recorded, current_boot_id())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn own_process_has_a_start_time_and_current_boot_matches() {
        let probe = OsProcessProbe;
        assert!(probe.start_time(std::process::id()).is_some());
        assert!(probe.same_boot(current_boot_id()));
        assert!(!probe.same_boot("linux:some-other-boot"));
        assert!(!probe.same_boot("boot-time:1"));
    }

    #[test]
    fn proc_stat_parsing_survives_odd_command_names() {
        // pid (comm) state ppid ... field 22 = starttime
        let tail = "S 1 100 100 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 200 18446744073709551615";
        let plain = format!("42 (bash) {tail}");
        assert_eq!(parse_proc_stat(&plain), Some(('S', 987_654)));
        let nasty = format!("42 (my (weird) name) {tail}");
        assert_eq!(parse_proc_stat(&nasty), Some(('S', 987_654)));
        let zombie = format!("42 (x) Z{}", &tail[1..]);
        assert_eq!(parse_proc_stat(&zombie), Some(('Z', 987_654)));
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("1 (a) S 1 2"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_stamp_is_ticks_since_boot_and_stable() {
        let probe = OsProcessProbe;
        let a = probe.start_time(std::process::id()).unwrap();
        let b = probe.start_time(std::process::id()).unwrap();
        assert_eq!(a, b);
        assert!(probe.same_process(a, b));
        assert!(
            !probe.same_process(a, a + 1),
            "exact comparison: no wall-clock tolerance"
        );
        // ticks since boot are far smaller than any epoch-based stamp
        assert!(a < 1_000_000_000_000, "{a}");
    }

    #[test]
    fn nonexistent_pid_has_no_start_time() {
        assert!(OsProcessProbe.start_time(u32::MAX - 7).is_none());
    }

    #[test]
    fn boot_time_tolerance_absorbs_clock_adjustments_only() {
        assert!(same_boot("boot-time:1000", "boot-time:1100"));
        assert!(!same_boot("boot-time:1000", "boot-time:5000"));
        assert!(same_boot("linux:abc", "linux:abc"));
        assert!(!same_boot("linux:abc", "linux:abd"));
        assert!(!same_boot("linux:abc", "boot-time:1"));
    }

    #[cfg(unix)]
    #[test]
    fn child_process_has_a_start_time_until_it_exits() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let probe = OsProcessProbe;
        let started = probe.start_time(child.id()).unwrap();
        let mine = probe.start_time(std::process::id()).unwrap();
        assert!(
            started >= mine.saturating_sub(1),
            "child cannot predate this test process"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(probe.start_time(child.id()).is_none());
    }
}
