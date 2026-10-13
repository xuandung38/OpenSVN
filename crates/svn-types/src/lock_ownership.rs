//! Ownership registry for working-copy directory locks (WC_LOCK rows carry no owner).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::error::Result;
use crate::rel_path::RelPath;

/// Records which process holds a WC_LOCK so stale rows left by crashed processes can be found.
pub trait LockOwnershipStore: Send + Sync {
    /// Records that `pid` (started in boot session `boot_id`) holds the lock on `dir`.
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

    /// Creates a store whose probe treats only the current process, in boot session
    /// `boot_id`, as alive. Suitable for tests; real probes live in the app layer.
    pub fn for_current_process(boot_id: &str) -> Self {
        let boot_id = boot_id.to_owned();
        Self::new(Arc::new(move |pid: u32, b: &str| {
            pid == std::process::id() && b == boot_id
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, (u32, String)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LockOwnershipStore for MemoryLockOwnership {
    fn record(&self, wcroot: &Path, dir: &RelPath, pid: u32, boot_id: &str) -> Result<()> {
        self.lock().insert(
            (wcroot.to_path_buf(), dir.clone()),
            (pid, boot_id.to_owned()),
        );
        Ok(())
    }

    fn forget(&self, wcroot: &Path, dir: &RelPath) -> Result<()> {
        self.lock().remove(&(wcroot.to_path_buf(), dir.clone()));
        Ok(())
    }

    fn owned_by_dead_process(&self, wcroot: &Path) -> Result<Vec<RelPath>> {
        let mut dead: Vec<RelPath> = self
            .lock()
            .iter()
            .filter(|((root, _), (pid, boot))| root == wcroot && !self.probe.is_alive(*pid, boot))
            .map(|((_, dir), _)| dir.clone())
            .collect();
        dead.sort();
        Ok(dead)
    }
}
