//! Durability barriers shared by the pristine store, the work queue and the update editor.
//!
//! The working copy keeps one ordering rule across power loss: a file is durable before the
//! wc.db transaction that relies on it commits (a NODES row naming a pristine text, the
//! completion of a work item that installed a working file). Syncing every file on its own costs
//! one `fsync` per file, which dominates a checkout of many small files.
//!
//! Where the platform can flush a whole filesystem in one call (Linux `syncfs`) and the working
//! copy lives on a local block filesystem that honours it ([`batched_sync`]), writers skip the
//! per-file sync and issue one [`sync_filesystem`] right before the transaction that relies on
//! their files commits. Everywhere else writers keep syncing each file.
//! A process kill (as opposed to power loss) never needs either: the page cache survives it.

use std::path::Path;

/// True when the platform can flush a whole filesystem in one call ([`sync_filesystem`]).
/// Whether a given working copy may rely on it is [`batched_sync`].
pub const BATCHED_SYNC: bool = cfg!(target_os = "linux");

/// True when writers below `path` may defer their per-file syncs to one [`sync_filesystem`]
/// barrier before the transaction that relies on the files commits.
///
/// Only for local block filesystems known to write back everything on `syncfs`: on FUSE
/// (sshfs, rclone, ntfs-3g) `syncfs` may never reach the daemon, and on network filesystems
/// (NFS, SMB/CIFS, 9p) it is weaker than a per-file `fsync` and flushes a shared volume. Those,
/// and anything unknown or unreadable, keep the per-file sync.
pub fn batched_sync(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::statfs(path).is_ok_and(|s| local_block_fs(s.f_type as u64))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

/// `statfs` magic numbers (`linux/magic.h`) of the filesystems [`batched_sync`] trusts.
#[cfg(any(target_os = "linux", test))]
const LOCAL_BLOCK_FS: [u32; 5] = [
    0xEF53,      // ext2, ext3, ext4
    0x5846_5342, // xfs
    0x9123_683E, // btrfs
    0xF2F5_2010, // f2fs
    0x0102_1994, // tmpfs (nothing to flush; no daemon in between)
];

/// Whether the `statfs` filesystem type `f_type` is one [`batched_sync`] trusts. The magic is
/// 32 bits wide; `f_type` is a signed word on some targets, so only the low 32 bits count.
/// Only Linux reads `statfs` for this; elsewhere it exists for the unit tests.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn local_block_fs(f_type: u64) -> bool {
    LOCAL_BLOCK_FS.contains(&(f_type as u32))
}

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

/// Syncs one existing file to disk (a file a filesystem barrier does not cover, or one written
/// through a handle that is gone).
///
/// Windows flushes only through a handle with write access (`FlushFileBuffers`), so the file is
/// opened for writing there, never truncated. A file with the read-only attribute cannot be
/// opened that way: it is left to the system's write-back, like directory entries, which
/// Windows cannot sync either.
pub fn sync_file(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(file) => file.sync_all(),
            Err(e)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    && std::fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(windows))]
    std::fs::File::open(path)?.sync_all()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_local_block_filesystems_batch() {
        for (name, magic) in [
            ("ext4", 0xEF53_u64),
            ("xfs", 0x5846_5342),
            ("btrfs", 0x9123_683E),
            ("f2fs", 0xF2F5_2010),
            ("tmpfs", 0x0102_1994),
        ] {
            assert!(local_block_fs(magic), "{name}");
        }
        for (name, magic) in [
            ("fuse", 0x6573_5546_u64),
            ("nfs", 0x6969),
            ("smb", 0x517B),
            ("cifs", 0xFF53_4D42),
            ("smb2", 0xFE53_4D42),
            ("9p", 0x0102_1997),
            ("overlay", 0x794C_7630),
            ("ecryptfs", 0xF15F),
            ("unknown", 0),
        ] {
            assert!(!local_block_fs(magic), "{name}");
        }
        // A sign-extended word (32-bit `f_type` read as negative) still matches.
        assert!(local_block_fs(0xFFFF_FFFF_9123_683E));
    }

    #[test]
    fn unreadable_paths_do_not_batch() {
        assert!(!batched_sync(Path::new("/nonexistent/opensvn/wc")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn batched_sync_follows_the_filesystem_type() {
        let dir = tempfile::tempdir().unwrap();
        let f_type = rustix::fs::statfs(dir.path()).unwrap().f_type as u64;
        assert_eq!(batched_sync(dir.path()), local_block_fs(f_type));
        // procfs is not a block filesystem.
        assert!(!batched_sync(Path::new("/proc")));
    }
}
