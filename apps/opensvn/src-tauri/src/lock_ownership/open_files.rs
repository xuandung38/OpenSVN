//! Which other processes have a file open: the safety check before the user removes the working
//! copy lock of another program. svn keeps `wc.db` open for as long as it holds a lock, so a
//! process that still has it open may still be writing the working copy.
//!
//! Only Linux can tell (from `/proc/<pid>/fd`); elsewhere the answer is "unknown" and the user's
//! explicit confirmation is all there is. Processes of other users whose `fd` directory cannot be
//! read are not seen.

use std::path::Path;

/// A process that has the file open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenBy {
    /// Process id.
    pub pid: u32,
    /// Command name (`/proc/<pid>/comm`), empty when unreadable.
    pub name: String,
}

/// Processes other than this one that have `file` open; `None` when this platform cannot tell.
pub fn other_processes_with_open(file: &Path) -> Option<Vec<OpenBy>> {
    #[cfg(target_os = "linux")]
    {
        Some(linux::scan(file))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = file;
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;

    use super::OpenBy;

    pub(super) fn scan(file: &Path) -> Vec<OpenBy> {
        let Ok(target) = std::fs::canonicalize(file) else {
            return Vec::new();
        };
        let me = std::process::id();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in procs.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == me {
                continue;
            }
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue; // gone, or another user's process
            };
            let open = fds
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|p| p == target));
            if open {
                let name = std::fs::read_to_string(entry.path().join("comm"))
                    .map(|n| n.trim().to_owned())
                    .unwrap_or_default();
                found.push(OpenBy { pid, name });
            }
        }
        found.sort_by_key(|o| o.pid);
        found
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn sees_a_child_that_holds_the_file_open_and_not_this_process() {
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wc.db");
        std::fs::write(&file, b"x").unwrap();
        let _mine = std::fs::File::open(&file).unwrap();
        assert_eq!(other_processes_with_open(&file), Some(Vec::new()));

        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 3<\"$1\"; exec sleep 30")
            .arg("sh")
            .arg(&file)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let seen = loop {
            let seen = other_processes_with_open(&file).unwrap();
            if !seen.is_empty() || Instant::now() > deadline {
                break seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].pid, child.id());
        assert_eq!(seen[0].name, "sleep");
        assert_eq!(other_processes_with_open(&file), Some(Vec::new()));
    }

    #[test]
    fn a_missing_file_is_open_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        let seen = other_processes_with_open(&dir.path().join("none"));
        assert!(seen.is_none_or(|s| s.is_empty()));
    }
}
