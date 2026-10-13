//! Private directories for generated files: previews (image diffs) and the copies handed to the
//! external diff tool. They live under the per-user data directory, never in a shared temp folder.
//! The preview folder is the only place the asset protocol may serve from (granted at runtime; the
//! static scope in `tauri.conf.json` is empty).

use std::path::{Path, PathBuf};

use opensvn_ipc::AppError;

/// Folder under the data directory holding previews.
pub const PREVIEW_DIR: &str = "preview";
/// Folder under the data directory holding the copies opened in the external diff tool.
pub const EXTERNAL_DIFF_DIR: &str = "external-diff";

fn refuse(name: &str, why: &str) -> AppError {
    crate::errors::invalid_argument(format!("The {name} folder is not safe to use: {why}."))
}

/// Creates (or validates) `<data_dir>/preview` and returns it. See [`prepare_private_dir`].
pub fn prepare_preview_dir(data_dir: &Path) -> Result<PathBuf, AppError> {
    prepare_private_dir(data_dir, PREVIEW_DIR)
}

/// Creates (or validates) `<data_dir>/<name>` and returns it. The folder must be a real
/// directory (not a symlink) that only the current user can access, and it must be writable by
/// us (which also rules out a folder owned by another user).
pub fn prepare_private_dir(data_dir: &Path, name: &str) -> Result<PathBuf, AppError> {
    let dir = data_dir.join(name);
    std::fs::create_dir_all(data_dir)
        .map_err(|e| refuse(name, &format!("cannot create the data folder ({e})")))?;
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(refuse(name, &format!("cannot create it ({e})"))),
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| refuse(name, &format!("{e}")))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(refuse(name, "it is a link or not a folder"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(refuse(name, "other users could access it"));
        }
    }
    // Proves we can write here (a folder of another user with open permissions fails the check
    // above; one with closed permissions fails here).
    let probe = dir.join(".write-probe");
    std::fs::write(&probe, b"").map_err(|e| refuse(name, &format!("it is not writable ({e})")))?;
    let _ = std::fs::remove_file(&probe);
    Ok(dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_private_folder_and_accepts_it_again() {
        let data = tempfile::tempdir().unwrap();
        let dir = prepare_preview_dir(data.path()).unwrap();
        assert!(dir.ends_with("preview") && dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(prepare_preview_dir(data.path()).unwrap(), dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_preview_folder_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(victim.path(), data.path().join("preview")).unwrap();
        assert!(prepare_preview_dir(data.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_open_to_other_users_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        let dir = data.path().join("preview");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_preview_dir(data.path()).is_err());
    }
}
