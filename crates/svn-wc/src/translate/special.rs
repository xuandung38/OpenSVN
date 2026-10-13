//! `svn:special` files. Only symlinks are defined: repo-normal form is `link <target>`.

use std::path::Path;

use svn_types::write_file_replacing;

use super::error::TranslateError;

/// Parsed repo-normal content of a special file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecialContent {
    /// `link <target>`: a symlink to `target` (only the first line is used).
    Link(Vec<u8>),
    /// Unknown special type: kept as a regular file with the content unchanged.
    Regular(Vec<u8>),
}

/// Interprets repo-normal bytes of a file with `svn:special` (`create_special_file_from_stream`).
pub fn parse(content: &[u8]) -> SpecialContent {
    let line_end = content
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(content.len());
    match content[..line_end].strip_prefix(b"link ") {
        Some(target) => SpecialContent::Link(target.to_vec()),
        None => SpecialContent::Regular(content.to_vec()),
    }
}

/// Repo-normal bytes of a symlink pointing at `target` (`link <target>`, no newline).
pub fn symlink_to_repo_normal(target: &[u8]) -> Vec<u8> {
    let mut v = b"link ".to_vec();
    v.extend_from_slice(target);
    v
}

/// Reads a special working file in repo-normal form (`svn_subst_read_specialfile`): the
/// `link <target>` text for a symlink, the raw content for a regular file.
pub fn read_repo_normal(path: &Path) -> Result<Vec<u8>, TranslateError> {
    let meta = std::fs::symlink_metadata(path).map_err(TranslateError::io("stat", path))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(TranslateError::io("read link", path))?;
        Ok(symlink_to_repo_normal(
            os_bytes(target.as_os_str()).as_slice(),
        ))
    } else {
        std::fs::read(path).map_err(TranslateError::io("read", path))
    }
}

/// Creates the working file for repo-normal `content` at `path`, replacing an existing file or
/// symlink atomically (a symlink at `path` is replaced, never followed).
///
/// `path` must already have been vetted with `svn_types::safe_join`; this function does not
/// check ancestor directories. Symlinks become real symlinks on Unix. Elsewhere, and for
/// unknown special types, the full repo-normal content is written to a regular file, as svn
/// does when symlinks are unsupported (unverified on Windows: no Windows CI).
pub fn install(path: &Path, content: &[u8]) -> Result<(), TranslateError> {
    #[cfg(unix)]
    if let SpecialContent::Link(target) = parse(content) {
        return install_symlink(path, &target);
    }
    write_file_replacing(path, content)?;
    Ok(())
}

/// Creates a symlink under a unique name next to `path` (`symlink(2)` fails if the name exists,
/// so it is exclusive) and renames it over `path`.
#[cfg(unix)]
fn install_symlink(path: &Path, target: &[u8]) -> Result<(), TranslateError> {
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".svn-special.{}.{n}.tmp", std::process::id()));
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), &tmp)
        .map_err(TranslateError::io("create", &tmp))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(TranslateError::Io {
            op: "rename to",
            path: path.to_path_buf(),
            source: e,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}
