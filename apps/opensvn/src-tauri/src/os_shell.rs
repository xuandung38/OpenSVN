//! Showing items in the operating system's file manager.

use std::path::Path;

use opensvn_ipc::AppError;

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
