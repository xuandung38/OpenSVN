//! Joining server-derived paths onto a working copy root without escaping it.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::admin_name::is_admin_dir_alias;
use crate::error::{ErrorCode, Result, SvnError};
use crate::rel_path::RelPath;

/// Joins `rel` under `root` and proves no intermediate component is a symlink.
///
/// Every intermediate directory is checked with `lstat` (no link following); a symlink
/// at any intermediate level fails with [`ErrorCode::UnsafePath`]. Components that do not
/// exist yet are accepted (the walk stops at the first missing one). `root` itself may be
/// a symlink: it is the trust anchor and is not canonicalized.
///
/// Segments that alias the administrative directory are rejected: `.svn` under Unicode case
/// folding of its NFKC form, with ignorable code points or trailing dots/spaces, and the NTFS
/// 8.3 short name `SVN~<n>`. Every segment must be exactly one normal path component and the
/// result is verified to lie under `root`, whatever the platform.
///
/// The last component is NOT checked and may be an existing symlink. **Callers must never
/// write through the returned path directly**: write a temporary file next to it and rename
/// over it (see [`write_file_replacing`]) so a symlink at the destination is replaced, not followed.
pub fn safe_join(root: &Path, rel: &RelPath) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    let comps: Vec<&str> = rel.components().collect();
    let mut exists = true;
    for (i, seg) in comps.iter().enumerate() {
        if is_admin_dir_alias(seg) {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                format!("administrative directory in {rel}"),
            ));
        }
        if !is_single_normal_component(seg) {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                format!("segment {seg:?} of {rel} is not a plain name"),
            ));
        }
        out.push(seg);
        let is_last = i + 1 == comps.len();
        if exists && !is_last {
            match fs::symlink_metadata(&out) {
                Ok(md) if md.file_type().is_symlink() => {
                    return Err(SvnError::new(
                        ErrorCode::UnsafePath,
                        format!("symlink in intermediate component of {rel}"),
                    ));
                }
                Ok(md) if !md.is_dir() => {
                    return Err(SvnError::new(
                        ErrorCode::UnsafePath,
                        format!("non-directory in intermediate component of {rel}"),
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => exists = false,
                Err(e) => return Err(SvnError::from_io(format!("lstat {}", out.display()), &e)),
            }
        }
    }
    if !out.starts_with(root) || out.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(SvnError::new(
            ErrorCode::UnsafePath,
            format!("{rel} escapes the root"),
        ));
    }
    Ok(out)
}

/// A segment must parse as exactly one `Component::Normal` on the running platform; this
/// excludes drive prefixes, root markers and `..` that `PathBuf::push` would act on.
fn is_single_normal_component(seg: &str) -> bool {
    let mut it = Path::new(seg).components();
    matches!(it.next(), Some(Component::Normal(_))) && it.next().is_none()
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `data` to `dest` without following a symlink at `dest`: a temporary file is
/// created in the same directory (exclusive create) and renamed over the destination.
/// The parent directory must already exist and have been vetted with [`safe_join`].
pub fn write_file_replacing(dest: &Path, data: &[u8]) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| SvnError::new(ErrorCode::UnsafePath, "destination has no parent"))?;
    let name = dest
        .file_name()
        .ok_or_else(|| SvnError::new(ErrorCode::UnsafePath, "destination has no file name"))?
        .to_string_lossy();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let io =
        |what: &str, e: std::io::Error| SvnError::from_io(format!("{what} {}", tmp.display()), &e);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| io("create", e))?;
    let result = f
        .write_all(data)
        .and_then(|()| f.sync_all())
        .map_err(|e| io("write", e))
        .and_then(|()| rename_over(&tmp, dest).map_err(|e| io("rename", e)));
    if result.is_err() {
        // Best effort cleanup; the original error is what matters.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Attempts of the Windows rename retry loop. With a 10 ms start doubling up to a 500 ms cap the
/// sleeps add up to roughly ten seconds, the budget of svn's `WIN32_RETRY_LOOP` for sharing
/// violations caused by antivirus and indexer handles.
#[cfg(windows)]
const RENAME_RETRY_ATTEMPTS: u32 = 25;

/// Atomically renames `from` over `to`, replacing an existing destination (a symlink at `to`
/// is replaced, not followed). Both paths must be on the same filesystem.
///
/// On Windows a read-only destination (every pristine, `svn:needs-lock` files) has its
/// attribute cleared first, and sharing violations / access denied caused by antivirus or
/// indexer handles are retried with a bounded exponential backoff (about ten seconds in
/// total), like `svn_io_file_rename2`. Elsewhere this is a single `rename`.
pub fn rename_replacing(from: &Path, to: &Path) -> Result<()> {
    rename_over(from, to).map_err(|e| {
        SvnError::from_io(format!("rename {} to {}", from.display(), to.display()), &e)
    })
}

/// Renames `from` over `to`. On Windows a read-only destination (`svn:needs-lock` files) makes
/// the rename fail, and antivirus or indexer handles cause short-lived sharing violations, so
/// the attribute is cleared first and the rename is retried briefly.
#[cfg(windows)]
fn rename_over(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Ok(md) = fs::symlink_metadata(to) {
        let mut perms = md.permissions();
        if perms.readonly() {
            // Windows only: this clears FILE_ATTRIBUTE_READONLY, it does not make the file world-writable.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(to, perms);
        }
    }
    let mut delay = std::time::Duration::from_millis(10);
    let mut last = None;
    for _ in 0..RENAME_RETRY_ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e)
                if matches!(e.kind(), ErrorKind::PermissionDenied)
                    || matches!(e.raw_os_error(), Some(32 | 33)) =>
            {
                last = Some(e);
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_millis(500));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename retries exhausted")))
}

#[cfg(not(windows))]
fn rename_over(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}
