//! `svn:executable` and `svn:needs-lock` file flags (`io_set_perms` in svn's io.c).
//!
//! On Unix the permission bits are edited exactly like svn: execute bits follow the matching
//! read bits, read-only clears write bits that have a matching read bit, and making a file
//! writable only touches the owner bits. On Windows only the read-only attribute is used and
//! the executable flag is ignored. Symlinks are never modified.

use std::fs;
use std::path::Path;

use super::error::TranslateError;

/// Applies `change` to the permissions of `path`; a no-op when nothing changes or `path` is a symlink.
fn update_perms(
    path: &Path,
    change: impl Fn(fs::Permissions) -> fs::Permissions,
) -> Result<(), TranslateError> {
    let meta = fs::symlink_metadata(path).map_err(TranslateError::io("stat", path))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let old = meta.permissions();
    let new = change(old.clone());
    if new == old {
        return Ok(());
    }
    fs::set_permissions(path, new).map_err(TranslateError::io("set permissions of", path))
}

/// Sets or clears the execute bits (`svn_io_set_file_executable`). No-op on Windows.
pub fn set_executable(path: &Path, executable: bool) -> Result<(), TranslateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        update_perms(path, |p| {
            let mut mode = p.mode();
            let read_to_exec = (mode & 0o444) >> 2;
            if executable {
                mode |= read_to_exec;
            } else {
                mode &= !read_to_exec;
            }
            fs::Permissions::from_mode(mode)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
        Ok(())
    }
}

/// Makes the file read-only (`svn_io_set_file_read_only`): the write bit is cleared for
/// every class whose read bit is set; on Windows the read-only attribute is set.
pub fn set_read_only(path: &Path) -> Result<(), TranslateError> {
    update_perms(path, |mut p| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = p.mode();
            p = fs::Permissions::from_mode(mode & !((mode & 0o444) >> 1));
        }
        #[cfg(not(unix))]
        p.set_readonly(true);
        p
    })
}

/// Makes the file writable for its owner (`svn_io_set_file_read_write`); group and other
/// bits are left alone.
pub fn set_read_write(path: &Path) -> Result<(), TranslateError> {
    update_perms(path, |mut p| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            p = fs::Permissions::from_mode(p.mode() | 0o600);
        }
        #[cfg(not(unix))]
        p.set_readonly(false);
        p
    })
}

/// Inputs of [`sync_with_props`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncInput {
    /// Node status is `normal` (not `added`).
    pub status_is_normal: bool,
    /// `svn:needs-lock` in the node's current (working) properties.
    pub needs_lock_working: bool,
    /// `svn:needs-lock` in the committed (pristine) properties.
    pub needs_lock_pristine: bool,
    /// The WC holds a lock token for the file.
    pub has_lock: bool,
    /// `svn:executable` in the working properties.
    pub executable: bool,
}

/// Syncs the flags of a normal/added file with its properties (`svn_wc__sync_flags_with_props`).
///
/// The file becomes writable unless it is a normal, unlocked file whose working props carry
/// `svn:needs-lock`; in that case it becomes read-only only when the committed props have it
/// too (an uncommitted `svn:needs-lock` leaves permissions alone). The execute bits follow
/// `svn:executable`.
pub fn sync_with_props(path: &Path, input: SyncInput) -> Result<(), TranslateError> {
    if !input.status_is_normal || !input.needs_lock_working || input.has_lock {
        set_read_write(path)?;
    } else if input.needs_lock_pristine {
        set_read_only(path)?;
    }
    set_executable(path, input.executable)
}
