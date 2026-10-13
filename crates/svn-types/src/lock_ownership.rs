//! Ownership registry for working-copy directory locks (WC_LOCK rows carry no owner).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::error::{ErrorCode, Result, SvnError};
use crate::rel_path::RelPath;

/// Records which process holds a WC_LOCK so stale rows left by crashed processes can be found.
///
/// `wcroot` must be the canonical working copy root (resolved symlinks, platform case); callers
/// pass the same value to every method. Implementations may lexically normalize it further.
pub trait LockOwnershipStore: Send + Sync {
    /// Records that `pid` (started in boot session `boot_id`) holds the lock on `dir`.
    /// Fails with [`ErrorCode::WcBusy`] when a different owner that is still alive holds it;
    /// a dead owner's record is replaced.
    fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()>;
    /// Removes the record for `dir`.
    fn forget(&self, wcroot: &Path, dir: &RelPath) -> Result<()>;
    /// Directories whose recorded owner no longer exists (pid dead or boot id differs).
    fn owned_by_dead_process(&self, wcroot: &Path) -> Result<Vec<RelPath>>;
}

/// Decides whether a recorded owner is still alive.
pub trait AliveProbe: Send + Sync {
    /// True when process `pid` of boot session `boot_id` is still running.
    fn is_alive(&self, pid: u32, boot_id: &str) -> bool;
}

impl<F: Fn(u32, &str) -> bool + Send + Sync> AliveProbe for F {
    fn is_alive(&self, pid: u32, boot_id: &str) -> bool {
        self(pid, boot_id)
    }
}

type Key = (PathBuf, RelPath);

/// Lexical normalization (drops `.` components and trailing separators).
fn key(wcroot: &Path, dir: &RelPath) -> Key {
    (wcroot.components().collect(), dir.clone())
}

/// In-memory [`LockOwnershipStore`] for tests and CLI use.
pub struct MemoryLockOwnership {
    probe: Arc<dyn AliveProbe>,
    entries: Mutex<HashMap<Key, (u32, String)>>,
}

impl MemoryLockOwnership {
    /// Creates a store using the given liveness probe.
    pub fn new(probe: Arc<dyn AliveProbe>) -> Self {
        Self {
            probe,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Creates a store with a conservative probe: the current process is alive, any process of
    /// the same boot session is assumed alive (this crate cannot inspect other processes, and
    /// wrongly declaring a live owner dead would let cleanup steal its lock), and a different
    /// `boot_id` means the owner is gone. Real process probes live in the app layer.
    pub fn for_current_process(boot_id: &str) -> Self {
        let boot_id = boot_id.to_owned();
        Self::new(Arc::new(move |_pid: u32, b: &str| b == boot_id))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, (u32, String)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LockOwnershipStore for MemoryLockOwnership {
    fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()> {
        let mut entries = self.lock();
        let k = key(wcroot, dir);
        if let Some((old_pid, old_boot)) = entries.get(&k)
            && (*old_pid != pid || old_boot != boot_id)
            && self.probe.is_alive(*old_pid, old_boot)
        {
            return Err(SvnError::new(
                ErrorCode::WcBusy,
                format!("{dir} is locked by process {old_pid}"),
            ));
        }
        entries.insert(k, (pid, boot_id.to_owned()));
        Ok(())
    }

    fn forget(&self, wcroot: &Path, dir: &RelPath) -> Result<()> {
        self.lock().remove(&key(wcroot, dir));
        Ok(())
    }

    fn owned_by_dead_process(&self, wcroot: &Path) -> Result<Vec<RelPath>> {
        let want: PathBuf = wcroot.components().collect();
        let mut dead: Vec<RelPath> = self
            .lock()
            .iter()
            .filter(|((root, _), (pid, boot))| *root == want && !self.probe.is_alive(*pid, boot))
            .map(|((_, dir), _)| dir.clone())
            .collect();
        dead.sort();
        Ok(dead)
    }
}
