//! Real process liveness probe for working copy lock owners (macOS, Linux, Windows).
//!
//! An owner is a `(pid, boot_id)` pair. The boot id tells a still-running process from a pid that
//! was recycled after a reboot.

use std::sync::OnceLock;

use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, System};

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
    /// Start time (seconds since the epoch) of the live, non-zombie process `pid`; `None` when no
    /// such process exists.
    fn start_time(&self, pid: u32) -> Option<u64>;
    /// Whether `recorded` names the boot session the machine is in now.
    fn same_boot(&self, recorded: &str) -> bool;
}

/// [`ProcessProbe`] backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsProcessProbe;

impl ProcessProbe for OsProcessProbe {
    fn start_time(&self, pid: u32) -> Option<u64> {
        let pid = Pid::from_u32(pid);
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid)
            .filter(|p| !matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead))
            .map(sysinfo::Process::start_time)
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
