//! Working copy plumbing shared by the operations: store adapters, change detection and the
//! single-transaction writers.

pub mod adapters;
pub mod inspect;
pub mod txn;

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use svn_types::{ErrorCode, RelPath, Result, SvnError, safe_join};
use svn_wc::db::{WcDb, sql_err};

/// `(repository root URL, UUID)` recorded in the working copy.
pub fn repos_identity(db: &WcDb) -> Result<(String, String)> {
    db.with_read(|c| {
        c.query_row("SELECT root, uuid FROM repository ORDER BY id LIMIT 1", [], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| SvnError::new(ErrorCode::Corrupt, "wc.db has no repository row"))
    })
}

/// `a/b` for two relative paths (`b` may be the root).
pub fn join_rel(a: &RelPath, b: &RelPath) -> Result<RelPath> {
    if a.is_root() {
        Ok(b.clone())
    } else if b.is_root() {
        Ok(a.clone())
    } else {
        RelPath::new(&format!("{a}/{b}"))
    }
}

/// `path` relative to `base`, or `None` when it is not below it.
pub fn strip_rel(path: &RelPath, base: &RelPath) -> Option<RelPath> {
    if !path.starts_with(base) {
        return None;
    }
    if base.is_root() {
        return Some(path.clone());
    }
    let rest = path.as_str().get(base.as_str().len()..)?.trim_start_matches('/');
    RelPath::new(rest).ok()
}

/// Absolute path of `rel` inside the working copy, refusing traversal and symlinked parents.
pub fn abs_path(db: &WcDb, rel: &RelPath) -> Result<PathBuf> {
    safe_join(db.wcroot(), rel)
}

/// Working copy relative path of an absolute `path` below `wcroot`.
pub fn rel_of(wcroot: &Path, path: &Path) -> Result<RelPath> {
    let stripped = path.strip_prefix(wcroot).map_err(|_| {
        SvnError::new(
            ErrorCode::NotAWorkingCopy,
            format!("'{}' is outside the working copy", path.display()),
        )
    })?;
    let mut parts = Vec::new();
    for c in stripped.components() {
        parts.push(c.as_os_str().to_str().ok_or_else(|| {
            SvnError::new(ErrorCode::UnsafePath, "path is not valid UTF-8")
        })?);
    }
    RelPath::new(&parts.join("/"))
}

/// Opens the working copy that contains `path` and registers the lock owner; recovers locks that
/// a dead process of this application left behind.
pub fn open_containing(
    owner: &svn_wc::db::LockOwner,
    path: &Path,
) -> Result<(WcDb, RelPath)> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| SvnError::from_io(format!("cannot resolve '{}'", path.display()), &e))?;
    let root = svn_wc::db::find_wcroot(&canonical)?;
    let (db, _) = WcDb::open_and_recover(&root, owner.clone())?;
    let rel = rel_of(&root, &canonical)?;
    Ok((db, rel))
}
