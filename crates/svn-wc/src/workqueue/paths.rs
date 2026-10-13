//! Resolution of work item operands to filesystem paths: the single admin-path contract.
//!
//! Most operands are working copy relpaths and go through `svn_types::safe_join`, which refuses
//! anything that touches the administrative directory. svn itself, however, writes operands
//! inside it, and a working copy interrupted by svn or TortoiseSVN is stuck unless those are
//! runnable. [`AdmPath`] accepts **exactly two shapes**:
//!
//! * `.svn/tmp/<one component>`: temporary files (interrupted `svn revert` / `svn merge` leave
//!   `file-install ... .svn/tmp/svn-XXXX` and `file-remove .svn/tmp/svn-XXXX`). Usable wherever
//!   svn documents a temporary operand: install source, remove, move, translate source and
//!   destination.
//! * `.svn/pristine/<2 hex>/<40 hex>.svn-base` (lowercase hex, directory equal to the first two
//!   digits of the checksum): pristine texts. svn 1.14.5 and TortoiseSVN 1.14 pass these
//!   straight to `file-translate` as the merge-left/right sources (`update_editor.c` and
//!   `preserve_pre_merge_files`); 1.15 sets the text aside into `.svn/tmp` first. A pristine is
//!   **read-only data**: it is accepted only as the SOURCE of `file-install` and
//!   `file-translate`, never as a destination or for remove/move.
//!
//! Both shapes require `.svn` and every directory below it on the way to be real directories
//! (a symlink is refused) and the final component not to be a symlink. This is working-copy-format
//! knowledge and deliberately lives in svn-wc, not in `svn-types`.

use std::fs;
use std::path::{Path, PathBuf};

use svn_types::{RelPath, safe_join};

use crate::db::ADM_DIR;

use super::WorkQueueError;

const TMP_DIR: &str = "tmp";

fn unsafe_path(relpath: &str, why: impl std::fmt::Display) -> WorkQueueError {
    WorkQueueError::UnsafePath(format!("{relpath}: {why}"))
}

/// Resolves a normal working copy relpath below `wc_root`, rejecting traversal, absolute and
/// drive paths, the administrative directory, and symlinked ancestors.
pub(crate) fn resolve_wc_path(wc_root: &Path, relpath: &str) -> Result<PathBuf, WorkQueueError> {
    let rel = RelPath::new(relpath).map_err(|e| unsafe_path(relpath, &e.message))?;
    safe_join(wc_root, &rel).map_err(|e| unsafe_path(relpath, &e.message))
}

/// Which admin path shape a relpath had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmKind {
    /// `.svn/tmp/<name>`.
    Tmp,
    /// `.svn/pristine/<xx>/<sha1>.svn-base` (read-only source).
    Pristine,
}

/// A validated path inside the administrative directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmPath {
    /// Absolute path.
    pub(crate) path: PathBuf,
    /// Shape.
    pub(crate) kind: AdmKind,
}

fn require_real_dir(dir: &Path, relpath: &str) -> Result<(), WorkQueueError> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_dir() => Ok(()),
        Ok(_) => Err(unsafe_path(
            relpath,
            format!("{} is not a real directory", dir.display()),
        )),
        Err(e) => Err(WorkQueueError::Io {
            op: "stat",
            path: dir.to_path_buf(),
            source: e,
        }),
    }
}

fn reject_symlink(file: &Path, relpath: &str) -> Result<(), WorkQueueError> {
    match fs::symlink_metadata(file) {
        Ok(m) if m.file_type().is_symlink() => {
            Err(unsafe_path(relpath, "final component is a symlink"))
        }
        // A missing file is fine: destinations do not exist yet, removals may have run already.
        _ => Ok(()),
    }
}

fn lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parses and validates `relpath` as an [`AdmPath`].
pub(crate) fn resolve_adm_path(wc_root: &Path, relpath: &str) -> Result<AdmPath, WorkQueueError> {
    let rest = relpath
        .strip_prefix(ADM_DIR)
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| unsafe_path(relpath, "not a path inside the administrative directory"))?;
    let adm = wc_root.join(ADM_DIR);
    if let Some(name) = rest.strip_prefix("tmp/") {
        if name.is_empty() || name.contains('/') {
            return Err(unsafe_path(
                relpath,
                "expected exactly one component below .svn/tmp",
            ));
        }
        // Reuse the strict server-name rules (also rejects `..`, `.svn` aliases, control
        // characters, device names, ADS) on the single component.
        let checked = RelPath::from_server(name).map_err(|e| unsafe_path(relpath, &e.message))?;
        if checked.components().count() != 1 || checked.as_str() != name {
            return Err(unsafe_path(relpath, "component is not in canonical form"));
        }
        let tmp = adm.join(TMP_DIR);
        require_real_dir(&adm, relpath)?;
        require_real_dir(&tmp, relpath)?;
        let path = tmp.join(name);
        reject_symlink(&path, relpath)?;
        return Ok(AdmPath {
            path,
            kind: AdmKind::Tmp,
        });
    }
    if let Some(rest) = rest.strip_prefix("pristine/") {
        let (dir, file) = rest
            .split_once('/')
            .ok_or_else(|| unsafe_path(relpath, "expected .svn/pristine/<xx>/<sha1>.svn-base"))?;
        let sha = file
            .strip_suffix(".svn-base")
            .ok_or_else(|| unsafe_path(relpath, "pristine file must end in .svn-base"))?;
        if !lower_hex(dir, 2) || !lower_hex(sha, 40) || !sha.starts_with(dir) {
            return Err(unsafe_path(
                relpath,
                "pristine name must be <2 hex>/<40 hex>.svn-base with matching prefix",
            ));
        }
        let pristine = adm.join("pristine");
        let pdir = pristine.join(dir);
        require_real_dir(&adm, relpath)?;
        require_real_dir(&pristine, relpath)?;
        require_real_dir(&pdir, relpath)?;
        let path = pdir.join(file);
        reject_symlink(&path, relpath)?;
        return Ok(AdmPath {
            path,
            kind: AdmKind::Pristine,
        });
    }
    Err(unsafe_path(
        relpath,
        "only .svn/tmp/<name> and .svn/pristine/<xx>/<sha1>.svn-base are accepted",
    ))
}

/// Resolves an operand that may be a working copy path or an [`AdmPath`].
///
/// `allow_pristine` must be true only for read-only SOURCE operands (`file-install` source,
/// `file-translate` source); `.svn/tmp/<name>` is accepted for every operand svn documents as
/// possibly temporary. Every other operand must use [`resolve_wc_path`].
pub(crate) fn resolve_operand(
    wc_root: &Path,
    relpath: &str,
    allow_pristine: bool,
) -> Result<PathBuf, WorkQueueError> {
    if relpath == ADM_DIR || relpath.starts_with(".svn/") {
        let adm = resolve_adm_path(wc_root, relpath)?;
        if adm.kind == AdmKind::Pristine && !allow_pristine {
            return Err(unsafe_path(
                relpath,
                "a pristine file may only be used as a read-only source",
            ));
        }
        Ok(adm.path)
    } else {
        resolve_wc_path(wc_root, relpath)
    }
}
