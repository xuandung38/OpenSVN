//! `svn copy` and `svn move` inside one working copy.

use std::fs;
use std::path::Path;

use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::workqueue::WorkItem;

use super::lock::write_op;
use super::paths::{abs_of, rel_of};
use super::state::{Schedule, is_versioned, node_info, subtree};
use crate::wc::{run_work_queue, skels};

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
        let mut unversioned = Vec::new();
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
                unversioned.extend(unversioned_children(db, &n, &src_abs)?.into_iter().map(
                    |name| {
                        let dst_abs = abs_of(db, &RelPath::new(&target)?).join(&name);
                        Ok((src_abs.join(&name), dst_abs))
                    },
                ));
                work.push(WorkItem::DirInstall { relpath: target });
            } else {
                work.push(WorkItem::FileTranslate {
                    local: target.clone(),
                    src: n.as_str().to_owned(),
                    dst: target,
                });
            }
        }
        let unversioned = unversioned.into_iter().collect::<Result<Vec<_>>>()?;
        db.op_copy(&s, &d, &skels(&work))?;
        // Like svn's `copy_versioned_dir`, unversioned items (ignored ones too) are copied
        // along and stay unversioned. They are not in wc.db, so they are copied directly once
        // the versioned tree exists.
        run_work_queue(db, cancel)?;
        for (from, to) in unversioned {
            cancel.check()?;
            copy_unversioned(&from, &to)?;
        }
        Ok(d)
    })
}

/// Names of the items on disk in versioned directory `rel` that wc.db does not know.
fn unversioned_children(db: &WcDb, rel: &RelPath, abs: &Path) -> Result<Vec<String>> {
    let versioned = db.children(rel)?;
    let Ok(rd) = fs::read_dir(abs) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name != svn_wc::db::ADM_DIR && !rel.child(name).is_ok_and(|c| versioned.contains(&c))
        })
        .collect();
    out.sort();
    Ok(out)
}

fn io_err(what: &str, path: &Path, e: &std::io::Error) -> SvnError {
    SvnError::from_io(format!("{what} '{}'", path.display()), e)
}

/// Copies an unversioned file, symlink (as a link, never followed) or directory tree.
fn copy_unversioned(from: &Path, to: &Path) -> Result<()> {
    let meta = from
        .symlink_metadata()
        .map_err(|e| io_err("stat", from, &e))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        #[cfg(unix)]
        {
            let target = fs::read_link(from).map_err(|e| io_err("read link", from, &e))?;
            std::os::unix::fs::symlink(&target, to).map_err(|e| io_err("create link", to, &e))?;
        }
        return Ok(());
    }
    if ft.is_dir() {
        fs::create_dir(to).map_err(|e| io_err("create directory", to, &e))?;
        let mut entries: Vec<_> = fs::read_dir(from)
            .map_err(|e| io_err("read directory", from, &e))?
            .flatten()
            .collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for e in entries {
            copy_unversioned(&e.path(), &to.join(e.file_name()))?;
        }
        return Ok(());
    }
    if ft.is_file() {
        fs::copy(from, to).map_err(|e| io_err("copy", from, &e))?;
    }
    Ok(())
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

/// Kind of what is on disk at `path`, symlinks not followed; `None` when nothing is there.
fn disk_kind(path: &Path) -> Option<NodeKind> {
    let meta = path.symlink_metadata().ok()?;
    let ft = meta.file_type();
    Some(if ft.is_symlink() {
        NodeKind::Symlink
    } else if ft.is_dir() {
        NodeKind::Dir
    } else {
        NodeKind::File
    })
}

/// Records that versioned `src` was moved to `dst` (both absolute) outside version control,
/// like TortoiseSVN's "Repair move": the rows `svn move` writes, without touching the disk, so
/// `dst` keeps its content and the change commits as a move.
///
/// `src` must be missing from disk or scheduled for deletion; `dst` must exist on disk with the
/// node kind of `src` and be unversioned, a plain addition (reverted first, like TortoiseSVN
/// does, so additions below it become unversioned again) or a copy of `src`. Recording the same
/// move again is a no-op. A mixed-revision directory is recorded as a copy plus a delete, like
/// `svn move --allow-mixed-revisions`. Returns what was recorded.
pub fn mark_move(
    db: &WcDb,
    cancel: &CancelToken,
    src: &Path,
    dst: &Path,
) -> Result<svn_wc::db::MarkMove> {
    write_op(db, cancel, || {
        let (s, d) = (rel_of(db, src)?, rel_of(db, dst)?);
        if s.is_root() || d.is_root() {
            return Err(invalid("the working copy root cannot be part of a move"));
        }
        let info = node_info(db, &s)?;
        if info.node.moved_to.as_ref() == Some(&d) {
            return Ok(svn_wc::db::MarkMove::AlreadyMoved);
        }
        let src_kind = db
            .read_base(&s)?
            .map(|b| b.kind)
            .ok_or_else(|| invalid(format!("'{s}' has no committed version to move")))?;
        if info.schedule != Schedule::Deleted && disk_kind(&abs_of(db, &s)).is_some() {
            return Err(invalid(format!(
                "'{s}' still exists on disk; only a missing or deleted item can be marked as moved"
            )));
        }
        let parent = d
            .parent()
            .ok_or_else(|| invalid("destination has no parent"))?;
        if !is_versioned(db, &parent)? || node_info(db, &parent)?.schedule == Schedule::Deleted {
            return Err(invalid(format!(
                "destination parent '{parent}' is not under version control"
            )));
        }
        let dst_abs = abs_of(db, &d);
        let found =
            disk_kind(&dst_abs).ok_or_else(|| invalid(format!("'{d}' does not exist on disk")))?;
        let same_kind = match src_kind {
            NodeKind::Dir => found == NodeKind::Dir,
            NodeKind::Symlink => found == NodeKind::Symlink || cfg!(windows),
            _ => found == NodeKind::File || (cfg!(windows) && found == NodeKind::Symlink),
        };
        if !same_kind {
            return Err(invalid(format!(
                "'{d}' is not of the same kind as '{s}' ({src_kind:?} expected)"
            )));
        }
        if found == NodeKind::Dir && dst_abs.join(svn_wc::db::ADM_DIR).exists() {
            return Err(invalid(format!(
                "'{d}' contains a separate working copy and cannot be marked as moved"
            )));
        }
        db.op_mark_move(&s, &d, &[])
    })
}
