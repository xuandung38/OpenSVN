//! `svn delete`: schedule nodes for deletion and remove them from disk.

use std::fs;
use std::path::{Path, PathBuf};

use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::workqueue::WorkItem;

use super::ignore::{DEFAULT_GLOBAL_IGNORES, is_ignored};
use super::lock::write_op;
use super::paths::{abs_of, rel_of};
use super::state::{Schedule, has_conflict, node_info, props_modified, subtree, text_modified};
use crate::wc::skels;

/// Options of [`delete`].
#[derive(Debug, Clone)]
pub struct DeleteOptions {
    /// Schedule the deletion but leave the files on disk. Like `svn rm --keep-local`, nothing
    /// is checked: nothing is removed from disk, so nothing can be lost.
    pub keep_local: bool,
    /// Delete even when the node has local modifications or unversioned content.
    pub force: bool,
    /// `[miscellany] global-ignores`: unversioned items matching these (or `svn:ignore` /
    /// `svn:global-ignores`) do not block the deletion and are removed with their directory,
    /// as svn does.
    pub global_ignores: Vec<String>,
}

impl Default for DeleteOptions {
    fn default() -> Self {
        Self {
            keep_local: false,
            force: false,
            global_ignores: DEFAULT_GLOBAL_IGNORES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

fn invalid(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

/// `SVN_ERR_CLIENT_MODIFIED` (E195006): svn's refusal to delete local modifications.
fn modified(abs: &Path) -> SvnError {
    invalid(format!(
        "'{}' has local modifications -- commit or revert them first; use force to override \
         this restriction (local modifications may be lost)",
        abs.display()
    ))
    .with_svn_code(195_006)
}

/// `SVN_ERR_UNVERSIONED_RESOURCE` (E200005).
fn unversioned(abs: &Path) -> SvnError {
    invalid(format!(
        "'{}' is not under version control; use force to override this restriction (local \
         modifications may be lost)",
        abs.display()
    ))
    .with_svn_code(200_005)
}

/// Schedules `targets` for deletion and removes them from disk (unless `keep_local`). Without
/// `force` or `keep_local`, a target whose subtree holds local modifications or unversioned
/// items that are not ignored is refused (svn's E195006 / E200005) and the whole call has no
/// effect.
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
    // svn checks only when the files are removed from disk (`svn_client__wc_delete`).
    if !opts.force && !opts.keep_local {
        for n in &nodes {
            check_deletable(db, n, opts)?;
        }
    }
    let mut work = Vec::new();
    if !opts.keep_local {
        // Like svn: once the checks passed, what is left on disk below a directory is either
        // unmodified, ignored or allowed by `force`, and the whole directory goes.
        work.push(if info.node.kind == NodeKind::Dir {
            WorkItem::DirRemove {
                relpath: rel.as_str().to_owned(),
                recursive: true,
            }
        } else {
            WorkItem::FileRemove {
                relpath: rel.as_str().to_owned(),
            }
        });
    }
    Ok((rel.clone(), work))
}

/// svn's `find_undeletables` status callback for one node: refuses conflicts, local text or
/// property modifications (a plain added file always counts as modified, a plain added
/// directory only when it has properties, an unmodified copy does not), obstructions and
/// unversioned items that are not ignored. Deleted and missing nodes pass.
fn check_deletable(db: &WcDb, n: &RelPath, opts: &DeleteOptions) -> Result<()> {
    let ni = node_info(db, n)?;
    let abs = abs_of(db, n);
    if has_conflict(db, n)? {
        return Err(modified(&abs));
    }
    if ni.schedule == Schedule::Deleted {
        return Ok(());
    }
    let Ok(meta) = abs.symlink_metadata() else {
        return Ok(()); // missing
    };
    let on_disk_dir = meta.file_type().is_dir();
    if on_disk_dir != (ni.node.kind == NodeKind::Dir) {
        return Err(invalid(format!(
            "'{}' is obstructed by an item of another kind; use force to delete it",
            abs.display()
        ))
        .with_svn_code(195_006));
    }
    let copied = ni.node.repos_relpath.is_some();
    let changed = match ni.schedule {
        Schedule::Added | Schedule::Replaced if !copied => {
            ni.node.kind != NodeKind::Dir || props_modified(db, n)?
        }
        _ => text_modified(db, &ni)? || props_modified(db, n)?,
    };
    if changed {
        return Err(modified(&abs));
    }
    if ni.node.kind != NodeKind::Dir {
        return Ok(());
    }
    let versioned = db.children(n)?;
    let Ok(rd) = fs::read_dir(&abs) else {
        return Ok(());
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if name == svn_wc::db::ADM_DIR {
            continue;
        }
        if n.child(&name).is_ok_and(|c| versioned.contains(&c)) {
            continue;
        }
        if !is_ignored(db, n, &name, &opts.global_ignores)? {
            return Err(unversioned(&abs.join(&name)));
        }
    }
    Ok(())
}
