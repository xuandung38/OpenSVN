//! Resolution of work item operands to filesystem paths.
//!
//! Most operands are working copy relpaths and go through `svn_types::safe_join`, which refuses
//! anything that touches the administrative directory. A few operands that svn itself writes are
//! temporary files inside it: `.svn/tmp/<name>` (interrupted `svn revert` and `svn merge` leave
//! `file-install ... .svn/tmp/svn-XXXX` and `file-remove .svn/tmp/svn-XXXX`). Those are accepted
//! by [`resolve_admin_tmp`], and only in the exact shape `.svn/tmp/<single component>`.
//! This resolver is working-copy-format knowledge and deliberately lives in svn-wc, not in
//! `svn-types`.

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

/// True when `relpath` names something below `.svn/tmp/`.
pub(crate) fn is_admin_tmp(relpath: &str) -> bool {
    relpath
        .strip_prefix(ADM_DIR)
        .and_then(|r| r.strip_prefix('/'))
        .and_then(|r| r.strip_prefix(TMP_DIR))
        .is_some_and(|r| r.starts_with('/'))
}

/// Resolves `.svn/tmp/<name>` exactly. `<name>` must be one component: no further separators,
/// `.`/`..`, NUL, `\`, `:` (alternate data streams), characters Windows cannot store, trailing
/// dot or space, or reserved device names. Neither `.svn` nor `.svn/tmp` may be a symlink; both
/// must already be real directories.
pub(crate) fn resolve_admin_tmp(wc_root: &Path, relpath: &str) -> Result<PathBuf, WorkQueueError> {
    let name = relpath
        .strip_prefix(ADM_DIR)
        .and_then(|r| r.strip_prefix('/'))
        .and_then(|r| r.strip_prefix(TMP_DIR))
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| unsafe_path(relpath, "not a path inside .svn/tmp"))?;
    if name.is_empty() || name.contains('/') {
        return Err(unsafe_path(
            relpath,
            "expected exactly one component below .svn/tmp",
        ));
    }
    // Reuse the strict server-name rules (also rejects `..`, `.svn` aliases, control characters,
    // device names, ADS) on the single component.
    let checked = RelPath::from_server(name).map_err(|e| unsafe_path(relpath, &e.message))?;
    if checked.components().count() != 1 || checked.as_str() != name {
        return Err(unsafe_path(relpath, "component is not in canonical form"));
    }
    let adm = wc_root.join(ADM_DIR);
    let tmp = adm.join(TMP_DIR);
    for dir in [&adm, &tmp] {
        match fs::symlink_metadata(dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => {
                return Err(unsafe_path(
                    relpath,
                    format!("{} is not a real directory", dir.display()),
                ));
            }
            Err(e) => {
                return Err(WorkQueueError::Io {
                    op: "stat",
                    path: dir.clone(),
                    source: e,
                });
            }
        }
    }
    Ok(tmp.join(name))
}

/// Resolves an operand that may be a working copy path or a `.svn/tmp/<name>` temporary.
/// Use only for operands svn documents as possibly temporary (install source, remove, move,
/// translate source and destination); every other operand must use [`resolve_wc_path`].
pub(crate) fn resolve_operand(wc_root: &Path, relpath: &str) -> Result<PathBuf, WorkQueueError> {
    if is_admin_tmp(relpath) {
        resolve_admin_tmp(wc_root, relpath)
    } else {
        resolve_wc_path(wc_root, relpath)
    }
}
