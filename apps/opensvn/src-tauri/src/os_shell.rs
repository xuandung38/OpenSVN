//! Showing items in the operating system's file manager.

use std::path::{Path, PathBuf};

use opensvn_ipc::AppError;
use svn_types::{RelPath, safe_join};

use crate::errors::invalid_argument;

/// Reveals a file or folder in the file manager. Tests substitute a recorder.
pub trait FolderRevealer: Send + Sync {
    /// Opens the file manager with `path` selected.
    fn reveal(&self, path: &Path) -> Result<(), AppError>;
}

/// Uses Explorer, Finder or the desktop's file manager.
pub struct NativeRevealer;

impl FolderRevealer for NativeRevealer {
    fn reveal(&self, path: &Path) -> Result<(), AppError> {
        tauri_plugin_opener::reveal_item_in_dir(path).map_err(|e| {
            crate::errors::invalid_argument("The file manager could not be opened.")
                .with_detail(e.to_string())
                .with_path(path.to_string_lossy())
        })
    }
}

/// Resolves `rel` (relative to the working copy `root`; empty = the root itself) to the path the
/// file manager may show.
///
/// The file manager resolves symbolic links before it selects an item, and a repository can
/// ship versioned symlinks (`svn:special`) that point anywhere (`docs -> ~/.ssh`). So every
/// component below the root must be a real file or folder, never a symlink (also when it points
/// inside the working copy or nowhere), and the canonical target must stay inside the canonical
/// root. The canonical path is returned so the file manager has nothing left to resolve.
pub fn reveal_target(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let missing = |p: &Path| {
        invalid_argument(
            "The item no longer exists. Locate the working copy or remove it from the list.",
        )
        .with_path(p.to_string_lossy())
    };
    let rel = if rel.is_empty() {
        RelPath::root()
    } else {
        RelPath::from_server(rel).map_err(|e| AppError::from(&e))?
    };
    let mut cur = root.to_path_buf();
    for seg in rel.components() {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(invalid_argument(
                    "This item is a symbolic link. Only real files and folders inside the working copy are shown.",
                )
                .with_path(rel.as_str()));
            }
            Ok(_) => {}
            Err(_) => return Err(missing(&cur)),
        }
    }
    let target = safe_join(root, &rel).map_err(|e| AppError::from(&e))?;
    let canon_root = dunce::canonicalize(root).map_err(|_| missing(root))?;
    let canon = dunce::canonicalize(&target).map_err(|_| missing(&target))?;
    if !canon.starts_with(&canon_root) {
        return Err(
            invalid_argument("This item is outside the working copy.").with_path(rel.as_str())
        );
    }
    Ok(canon)
}
