//! Joining server-derived paths onto a working copy root without escaping it.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ErrorCode, Result, SvnError};
use crate::rel_path::RelPath;

/// Joins `rel` under `root` and proves no intermediate component is a symlink.
///
/// Every intermediate directory is checked with `lstat` (no link following); a symlink
/// at any intermediate level fails with [`ErrorCode::UnsafePath`]. Components that do not
/// exist yet are accepted (the walk stops at the first missing one). `root` itself may be
/// a symlink: it is the trust anchor and is not canonicalized. `.svn` segments are rejected.
///
/// The last component is NOT checked and may be an existing symlink. **Callers must never
/// write through the returned path directly**: write a temporary file next to it and rename
/// over it (see [`write_file_replacing`]) so a symlink at the destination is replaced, not followed.
pub fn safe_join(root: &Path, rel: &RelPath) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    let comps: Vec<&str> = rel.components().collect();
    let mut exists = true;
    for (i, seg) in comps.iter().enumerate() {
        if seg.eq_ignore_ascii_case(".svn") {
            return Err(SvnError::new(
                ErrorCode::UnsafePath,
                format!("administrative directory in {rel}"),
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
    Ok(out)
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
        .and_then(|()| fs::rename(&tmp, dest).map_err(|e| io("rename", e)));
    if result.is_err() {
        // Best effort cleanup; the original error is what matters.
        let _ = fs::remove_file(&tmp);
    }
    result
}
