//! `svn revert`: drop local changes and restore the working files from the pristine store.

use std::path::PathBuf;

use svn_types::{CancelToken, NodeKind, RelPath, Result};
use svn_wc::WcDb;
use svn_wc::conflicts::ConflictSkel;
use svn_wc::workqueue::WorkItem;

use super::lock::write_op;
use super::paths::{abs_of, rel_of};
use super::state::{Schedule, node_info, props_modified, subtree, text_modified};
use crate::wc::skels;

/// Options of [`revert`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RevertOptions {
    /// Revert the whole subtree of each target.
    pub recursive: bool,
}

/// Reverts `targets`. Unversioned targets without recorded state are ignored, like svn.
/// Returns the paths whose state was reverted.
pub fn revert(
    db: &WcDb,
    cancel: &CancelToken,
    targets: &[PathBuf],
    opts: &RevertOptions,
) -> Result<Vec<RelPath>> {
    write_op(db, cancel, || {
        let mut reverted = Vec::new();
        for t in targets {
            cancel.check()?;
            let rel = rel_of(db, t)?;
            let known = db.read_node(&rel)?.is_some()
                || db.conflict(&rel)?.is_some()
                || db.changelist(&rel)?.is_some();
            if !known {
                continue;
            }
            let nodes = if opts.recursive {
                subtree(db, &rel)?
            } else {
                vec![rel.clone()]
            };
            let mut work = Vec::new();
            for n in &nodes {
                restore_items(db, n, &mut work)?;
            }
            db.op_revert(&rel, opts.recursive, &skels(&work))?;
            reverted.extend(nodes);
        }
        Ok(reverted)
    })
}

/// Work needed to bring node `n` back to its pristine state on disk once its rows are reverted.
fn restore_items(db: &WcDb, n: &RelPath, work: &mut Vec<WorkItem>) -> Result<()> {
    let relpath = n.as_str().to_owned();
    // Conflict marker files disappear with the conflict.
    if let Some(rec) = db.conflict(n)?
        && let Ok(skel) = ConflictSkel::from_skel(&rec.skel)
    {
        let mut markers: Vec<String> = Vec::new();
        if let Some(t) = skel.text() {
            markers.extend(
                [&t.original, &t.mine, &t.their]
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        if let Some(p) = skel.prop() {
            markers.extend(p.markers.iter().cloned());
        }
        work.extend(
            markers
                .into_iter()
                .map(|relpath| WorkItem::FileRemove { relpath }),
        );
    }
    let Ok(info) = node_info(db, n) else {
        return Ok(());
    };
    let abs = abs_of(db, n);
    let on_disk = abs.symlink_metadata().is_ok();
    // After the revert the node shows its BASE state; added nodes simply become unversioned.
    let has_base = info.base.as_ref().is_some_and(|b| b.presence.is_present());
    let restores_base = match info.schedule {
        Schedule::Deleted | Schedule::Replaced => has_base,
        Schedule::Normal => !on_disk || props_modified(db, n)? || text_modified(db, &info)?,
        Schedule::Added => false,
    };
    if !restores_base {
        return Ok(());
    }
    let kind = info.base.as_ref().map_or(info.node.kind, |b| b.kind);
    if kind == NodeKind::Dir {
        if !on_disk {
            work.push(WorkItem::DirInstall { relpath });
        }
    } else {
        work.push(WorkItem::FileInstall {
            relpath,
            use_commit_times: false,
            record_fileinfo: true,
            source: None,
        });
    }
    Ok(())
}
