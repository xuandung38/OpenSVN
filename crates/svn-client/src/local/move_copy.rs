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
