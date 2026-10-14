//! Working copy plumbing shared by the operations: path helpers, change detection and the
//! working copy discovery. All wc.db access goes through the `svn-wc` facade (`WcDb`, `WcTxn`).

pub mod inspect;

use std::path::{Path, PathBuf};

use svn_types::{CancelToken, ErrorCode, RelPath, Result, SvnError, safe_join};
use svn_wc::db::{DbWorkingCopyOps, WcDb};
use svn_wc::skel::Skel;
use svn_wc::workqueue::WorkItem;

use crate::commit::file_commit::file_commit_fixup;

/// `(repository root URL, UUID)` of the working copy root.
pub fn repos_identity(db: &WcDb) -> Result<(String, String)> {
    let info = db.read_repos_info(&RelPath::root())?;
    Ok((info.repos_root_url, info.repos_uuid))
}

/// Runs every pending work item of `db` (see [`WcDb::run_work_queue_with`]); `cancel` is polled
/// between items and the unfinished items stay queued.
///
/// `file-commit` items (queued by a commit, possibly left behind by a crash) are handled by
/// [`crate::commit::file_commit::file_commit_fixup`], so every operation that finishes leftover
/// work also completes an interrupted commit.
pub fn run_work_queue(db: &WcDb, cancel: &CancelToken) -> Result<usize> {
    let cancelled = || cancel.is_cancelled();
    let mut ops = DbWorkingCopyOps::new(db).with_file_commit(file_commit_fixup);
    db.run_work_queue_with(&mut ops, Some(&cancelled))
}

/// [`run_work_queue`] that leaves the completion of the self-contained items to the caller
/// (see [`WcDb::run_work_queue_unsettled`]); `carried` are such items from an earlier run.
pub fn run_work_queue_unsettled(
    db: &WcDb,
    cancel: &CancelToken,
    carried: svn_wc::workqueue::Unsettled,
) -> Result<svn_wc::workqueue::Unsettled> {
    let cancelled = || cancel.is_cancelled();
    let mut ops = DbWorkingCopyOps::new(db).with_file_commit(file_commit_fixup);
    db.run_work_queue_unsettled(&mut ops, Some(&cancelled), carried)
        .map(|(_, unsettled)| unsettled)
}

/// Serializes `items` for the `WcDb` writers that queue work in their own transaction.
pub fn skels(items: &[WorkItem]) -> Vec<Skel> {
    items.iter().map(WorkItem::to_skel).collect()
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
    let rest = path
        .as_str()
        .get(base.as_str().len()..)?
        .trim_start_matches('/');
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
        parts.push(
            c.as_os_str()
                .to_str()
                .ok_or_else(|| SvnError::new(ErrorCode::UnsafePath, "path is not valid UTF-8"))?,
        );
    }
    RelPath::new(&parts.join("/"))
}

/// Opens the working copy that contains `path` and registers the lock owner; recovers locks that
/// a dead process of this application left behind.
pub fn open_containing(owner: &svn_wc::db::LockOwner, path: &Path) -> Result<(WcDb, RelPath)> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| SvnError::from_io(format!("cannot resolve '{}'", path.display()), &e))?;
    let root = svn_wc::db::find_wcroot(&canonical)?;
    let (db, _) = WcDb::open_and_recover(&root, owner.clone())?;
    let rel = rel_of(&root, &canonical)?;
    Ok((db, rel))
}

/// `svn upgrade` to working copy format `to`. Only the format the working copy already has is
/// accepted (a no-op): OpenSVN writes format 31 only, which svn 1.8 to 1.15 and TortoiseSVN use;
/// converting between formats 31 and 32 (`svn upgrade --compatible-version 1.15`) is not
/// supported and fails with [`ErrorCode::WcFormatUnsupported`] without touching anything.
pub fn upgrade_format(db: &WcDb, to: u32) -> Result<()> {
    let current = db.format();
    if to == current {
        return Ok(());
    }
    Err(SvnError::new(
        ErrorCode::WcFormatUnsupported,
        format!(
            "changing the working copy format from {current} to {to} is not supported; \
             OpenSVN writes format {} only. Check out a new working copy with a client that \
             writes format {to} if you need it",
            svn_wc::db::FORMAT_WRITE
        ),
    ))
}
