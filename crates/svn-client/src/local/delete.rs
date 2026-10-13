//! `svn delete`: schedule nodes for deletion and remove them from disk.

use std::fs;
use std::path::PathBuf;

use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::workqueue::WorkItem;

use super::lock::write_op;
use super::paths::{abs_of, rel_of};
use super::state::{Schedule, node_info, props_modified, subtree, text_modified};
use super::wc_adapters::skels;

/// Options of [`delete`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DeleteOptions {
    /// Schedule the deletion but leave the files on disk.
    pub keep_local: bool,
    /// Delete even when the node has local modifications or unversioned content.
    pub force: bool,
}

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// Schedules `targets` for deletion. Without `force` a node with local modifications, or a
/// directory holding unversioned content, is refused and nothing is changed for that target.
pub fn delete(
    db: &WcDb,
    cancel: &CancelToken,
    targets: &[PathBuf],
    opts: &DeleteOptions,
) -> Result<()> {
    write_op(db, cancel, || {
        // Validate every target first so a refusal leaves the whole call without effect.
        let mut plans = Vec::new();
        for t in targets {
            let rel = rel_of(db, t)?;
            plans.push(plan(db, &rel, opts)?);
        }
        for (rel, work) in plans {
            cancel.check()?;
            db.op_delete(&rel, &skels(&work))?;
        }
        Ok(())
    })
}

fn plan(db: &WcDb, rel: &RelPath, opts: &DeleteOptions) -> Result<(RelPath, Vec<WorkItem>)> {
    if rel.is_root() {
        return Err(invalid("cannot delete the working copy root"));
    }
    let info = node_info(db, rel)?;
    if info.schedule == Schedule::Deleted {
        return Err(invalid(format!(
            "'{rel}' is already scheduled for deletion"
        )));
    }
    let nodes = subtree(db, rel)?;
    if !opts.force {
        for n in &nodes {
            let ni = node_info(db, n)?;
            if ni.schedule == Schedule::Deleted {
                continue;
            }
            if text_modified(db, &ni)? && ni.schedule == Schedule::Normal
                || props_modified(db, n)? && ni.schedule == Schedule::Normal
            {
                return Err(invalid(format!(
                    "'{n}' has local modifications; use force to delete it"
                )));
            }
            if ni.node.kind == NodeKind::Dir {
                let versioned = db.children(n)?;
                let abs = abs_of(db, n);
                if let Ok(rd) = fs::read_dir(&abs) {
                    for entry in rd.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name == svn_wc::db::ADM_DIR {
                            continue;
                        }
                        let known = n.child(&name).is_ok_and(|c| versioned.contains(&c));
                        if !known && !opts.keep_local {
                            return Err(invalid(format!(
                                "'{n}' contains unversioned item '{name}'; use force to delete it"
                            )));
                        }
                    }
                }
            }
        }
    }
    let mut work = Vec::new();
    if !opts.keep_local {
        if opts.force && info.node.kind == NodeKind::Dir {
            work.push(WorkItem::DirRemove {
                relpath: rel.as_str().to_owned(),
                recursive: true,
            });
        } else {
            // Files first, then directories deepest first (non-recursive removal tolerates
            // directories that still hold something).
            for n in &nodes {
                let ni = node_info(db, n)?;
                if ni.schedule != Schedule::Deleted && ni.node.kind != NodeKind::Dir {
                    work.push(WorkItem::FileRemove {
                        relpath: n.as_str().to_owned(),
                    });
                }
            }
            for n in nodes.iter().rev() {
                let ni = node_info(db, n)?;
                if ni.schedule != Schedule::Deleted && ni.node.kind == NodeKind::Dir {
                    work.push(WorkItem::DirRemove {
                        relpath: n.as_str().to_owned(),
                        recursive: false,
                    });
                }
            }
        }
    }
    Ok((rel.clone(), work))
}
