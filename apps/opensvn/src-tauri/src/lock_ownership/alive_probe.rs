//! Real process liveness probe for working copy lock owners (macOS, Linux, Windows).
//!
//! An owner is a `(pid, boot_id)` pair. The boot id tells a still-running process from a pid that
//! was recycled after a reboot.

use std::sync::OnceLock;

use svn_types::AliveProbe;
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

/// True when a live (non-zombie) process with this pid exists.
fn process_alive(pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .is_some_and(|p| !matches!(p.status(), ProcessStatus::Zombie | ProcessStatus::Dead))
}

/// [`AliveProbe`] backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsAliveProbe;

impl AliveProbe for OsAliveProbe {
    fn is_alive(&self, pid: u32, boot_id: &str) -> bool {
        if !same_boot(boot_id, current_boot_id()) {
            return false;
        }
        pid == std::process::id() || process_alive(pid)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_and_other_boot_is_dead() {
        let probe = OsAliveProbe;
        assert!(probe.is_alive(std::process::id(), current_boot_id()));
        assert!(!probe.is_alive(std::process::id(), "linux:some-other-boot"));
        assert!(!probe.is_alive(std::process::id(), "boot-time:1"));
    }

    #[test]
    fn nonexistent_pid_is_dead() {
        assert!(!OsAliveProbe.is_alive(u32::MAX - 7, current_boot_id()));
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
    fn child_process_is_alive_until_it_exits() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let probe = OsAliveProbe;
        assert!(probe.is_alive(child.id(), current_boot_id()));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!probe.is_alive(child.id(), current_boot_id()));
    }
}
