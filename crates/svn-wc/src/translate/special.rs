//! `svn:special` files. Only symlinks are defined: repo-normal form is `link <target>`.

use std::path::Path;

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

/// Creates the working file for repo-normal `content` at `path`, replacing an existing file
/// atomically. Symlinks become real symlinks on Unix; elsewhere (and for unknown special
/// types) the repo-normal text is written to a regular file, as svn does.
pub fn install(path: &Path, content: &[u8]) -> Result<(), TranslateError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".svn-special")
        .tempfile_in(dir)
        .map_err(TranslateError::io("create temp file in", dir))?;
    let tmp_path = tmp.into_temp_path();
    // The temp file name must be free for the symlink; keep only the unique name.
    std::fs::remove_file(&tmp_path).map_err(TranslateError::io("remove", &tmp_path))?;
    let created = match parse(content) {
        SpecialContent::Link(target) => make_symlink(&target, &tmp_path),
        SpecialContent::Regular(bytes) => std::fs::write(&tmp_path, bytes),
    };
    created.map_err(TranslateError::io("create", &tmp_path))?;
    std::fs::rename(&tmp_path, path).map_err(TranslateError::io("rename to", path))?;
    // `tmp_path` was consumed by the rename; its drop only removes a missing path.
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &[u8], at: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), at)
}

#[cfg(not(unix))]
fn make_symlink(target: &[u8], at: &Path) -> std::io::Result<()> {
    // No symlink support: fall back to the internal representation.
    std::fs::write(at, super::special::symlink_to_repo_normal(target))
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
