//! `svn copy` and `svn move` inside one working copy.

use std::path::Path;

use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::workqueue::WorkItem;

use super::lock::write_op;
use super::paths::{abs_of, rel_of};
use super::state::{Schedule, is_versioned, node_info, subtree};
use crate::wc::skels;

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// Checks that `src` can be copied or moved to `dst`.
fn check_endpoints(db: &WcDb, src: &RelPath, dst: &RelPath) -> Result<()> {
    if src.is_root() || dst.is_root() {
        return Err(invalid("the working copy root cannot be copied or moved"));
    }
    if dst.starts_with(src) {
        return Err(invalid(format!("cannot copy or move '{src}' into itself")));
    }
    let info = node_info(db, src)?;
    if info.schedule == Schedule::Deleted {
        return Err(invalid(format!("'{src}' is scheduled for deletion")));
    }
    let parent = dst
        .parent()
        .ok_or_else(|| invalid("destination has no parent"))?;
    if !is_versioned(db, &parent)? {
        return Err(invalid(format!(
            "destination parent '{parent}' is not under version control"
        )));
    }
    if abs_of(db, dst).symlink_metadata().is_ok() {
        return Err(invalid(format!("'{dst}' already exists on disk")));
    }
    if is_versioned(db, dst)? && node_info(db, dst)?.schedule != Schedule::Deleted {
        return Err(invalid(format!("'{dst}' is already under version control")));
    }
    Ok(())
}

/// Copies versioned `src` to `dst` (both absolute) with history; the copy is scheduled for
/// addition. Local modifications of `src` are copied too. Returns the new relative path.
pub fn copy(db: &WcDb, cancel: &CancelToken, src: &Path, dst: &Path) -> Result<RelPath> {
    write_op(db, cancel, || {
        let (s, d) = (rel_of(db, src)?, rel_of(db, dst)?);
        check_endpoints(db, &s, &d)?;
        let mut work = Vec::new();
        for n in subtree(db, &s)? {
            let ni = node_info(db, &n)?;
            if ni.schedule == Schedule::Deleted {
                continue;
            }
            let suffix = n.as_str().strip_prefix(s.as_str()).unwrap_or("");
            let target = format!("{}{}", d.as_str(), suffix);
            let src_abs = abs_of(db, &n);
            if src_abs.symlink_metadata().is_err() {
                return Err(invalid(format!(
                    "'{n}' is missing from disk; restore it first"
                )));
            }
            if ni.node.kind == NodeKind::Dir {
                work.push(WorkItem::DirInstall { relpath: target });
            } else {
                work.push(WorkItem::FileTranslate {
                    local: target.clone(),
                    src: n.as_str().to_owned(),
                    dst: target,
                });
            }
        }
        db.op_copy(&s, &d, &skels(&work))?;
        Ok(d)
    })
}

/// Moves versioned `src` to `dst` (both absolute): the old location is scheduled for deletion
/// and the new one for addition with history, linked as a move. Returns the new relative path.
pub fn move_path(db: &WcDb, cancel: &CancelToken, src: &Path, dst: &Path) -> Result<RelPath> {
    write_op(db, cancel, || {
        let (s, d) = (rel_of(db, src)?, rel_of(db, dst)?);
        check_endpoints(db, &s, &d)?;
        if abs_of(db, &s).symlink_metadata().is_err() {
            return Err(invalid(format!(
                "'{s}' is missing from disk; restore it first"
            )));
        }
        let work = [WorkItem::FileMove {
            src: s.as_str().to_owned(),
            dst: d.as_str().to_owned(),
        }];
        db.op_move(&s, &d, &skels(&work))?;
        Ok(d)
    })
}
