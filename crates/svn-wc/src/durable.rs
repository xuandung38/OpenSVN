//! Durability barriers shared by the pristine store, the work queue and the update editor.
//!
//! The working copy keeps one ordering rule across power loss: a file is durable before the
//! wc.db transaction that relies on it commits (a NODES row naming a pristine text, the
//! completion of a work item that installed a working file). Syncing every file on its own costs
//! one `fsync` per file, which dominates a checkout of many small files.
//!
//! Where the platform can flush a whole filesystem in one call (Linux `syncfs`), writers skip the
//! per-file sync and issue one [`sync_filesystem`] right before the transaction that relies on
//! their files commits. Elsewhere [`BATCHED_SYNC`] is false and writers keep syncing each file.
//! A process kill (as opposed to power loss) never needs either: the page cache survives it.

use std::path::Path;

/// True when [`sync_filesystem`] flushes a whole filesystem in one call, so writers may defer
/// their per-file syncs to one barrier before the transaction that relies on the files commits.
pub const BATCHED_SYNC: bool = cfg!(target_os = "linux");

/// Flushes every dirty file and directory entry of the filesystem holding `path` to disk
/// (`syncfs`). A no-op where [`BATCHED_SYNC`] is false (writers synced each file already).
pub fn sync_filesystem(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let dir = std::fs::File::open(path)?;
        rustix::fs::syncfs(&dir)?;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    Ok(())
}

/// Device id of `path`, to find files that live on another filesystem than the barrier covers.
#[cfg(unix)]
pub(crate) fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

/// Device ids are not compared off Unix (no batched barrier there).
#[cfg(not(unix))]
pub(crate) fn device_of(_path: &Path) -> Option<u64> {
    None
}

/// Syncs one file that a filesystem barrier does not cover (another device below the root).
pub(crate) fn sync_file(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}
