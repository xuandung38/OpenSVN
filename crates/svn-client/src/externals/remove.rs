//! Removing externals whose definition is gone (svn's `handle_external_item_removal`), without
//! ever deleting a local change: see the module documentation of [`super`].

use std::path::Path;
use std::sync::Arc;

use svn_types::{
    CancelToken, Depth, ErrorCode, NodeKind, NodeStatus, PropStatus, RelPath, Result, SvnError,
    WcStatus,
};
use svn_wc::db::{BaseNode, External, LockOwner, Node, ObtainMode, Presence, WcDb};
use svn_wc::workqueue::WorkItem;

use super::{DirLock, ExternalAction, blocking, report};
use crate::ctx::{ClientCtx, ExternalReport};
use crate::status::{StatusOptions, is_wc_root_dir, local_status};
use crate::wc::{self, inspect};

/// How removing a directory external ended.
enum Removal {
    /// Nothing of it is left on disk.
    Removed,
    /// Something was left in place; the text says what and why.
    Left(String),
}

/// Removes the recorded external `ext` of `db` and drops its registration. Local changes are
/// kept and reported as [`ExternalAction::LeftLocalModifications`].
pub(super) async fn remove(
    ctx: &ClientCtx,
    db: &Arc<WcDb>,
    ext: &External,
    cancel: &CancelToken,
) -> Result<ExternalReport> {
    let (db, ext, cancel, owner, globals) = (
        Arc::clone(db),
        ext.clone(),
        cancel.clone(),
        ctx.lock_owner.clone(),
        ctx.config.global_ignores.clone(),
    );
    blocking(move || {
        let r = remove_blocking(&db, &ext, &owner, globals.as_deref(), &cancel)?;
        if r.action == ExternalAction::Removed {
            prune_empty_parents(&db, &ext.local_relpath)?;
        }
        Ok(r)
    })
    .await
}

/// Removes the directories left empty above a removed external, up to the nearest versioned
/// one (an update that deleted the defining directory could not remove it while the external
/// was still inside).
fn prune_empty_parents(db: &WcDb, local: &RelPath) -> Result<()> {
    let mut cur = local.parent();
    while let Some(dir) = cur {
        if dir.is_root() || db.read_node(&dir)?.is_some_and(|n| n.presence.is_present()) {
            break;
        }
        let abs = wc::abs_path(db, &dir)?;
        if !abs.symlink_metadata().is_ok_and(|m| m.is_dir()) || std::fs::remove_dir(&abs).is_err() {
            break;
        }
        cur = dir.parent();
    }
    Ok(())
}

fn remove_blocking(
    db: &Arc<WcDb>,
    ext: &External,
    owner: &LockOwner,
    globals: Option<&[String]>,
    cancel: &CancelToken,
) -> Result<ExternalReport> {
    match ext.kind {
        NodeKind::File | NodeKind::Symlink => remove_file_external(db, ext, cancel),
        _ => {
            let abs = wc::abs_path(db, &ext.local_relpath)?;
            let removal = remove_wc(owner, &abs, globals, cancel)?;
            {
                let _lock = lock_nearest(db, &ext.def_local_relpath, cancel)?;
                db.with_txn(|t| t.external_remove(&ext.local_relpath))?;
            }
            Ok(match removal {
                Removal::Removed => report(
                    &ext.local_relpath,
                    NodeKind::Dir,
                    ExternalAction::Removed,
                    None,
                ),
                Removal::Left(why) => ExternalReport {
                    message: Some(format!(
                        "left local modifications as '{}': {why}",
                        ext.local_relpath
                    )),
                    ..report(
                        &ext.local_relpath,
                        NodeKind::Dir,
                        ExternalAction::LeftLocalModifications,
                        None,
                    )
                },
            })
        }
    }
}

/// Locks `dir`, or its nearest versioned ancestor when `dir` is gone (a defining directory the
/// update deleted).
fn lock_nearest(db: &Arc<WcDb>, dir: &RelPath, cancel: &CancelToken) -> Result<DirLock> {
    let mut cur = Some(dir.clone());
    while let Some(d) = cur {
        if db.read_base(&d)?.is_some_and(|n| n.kind == NodeKind::Dir) {
            return DirLock::obtain(db, &d, cancel);
        }
        cur = d.parent();
    }
    DirLock::obtain(db, &RelPath::root(), cancel)
}

/// A status entry that removing the external would lose.
fn is_local_change(s: &WcStatus) -> bool {
    matches!(
        s.text,
        NodeStatus::Added
            | NodeStatus::Deleted
            | NodeStatus::Modified
            | NodeStatus::Replaced
            | NodeStatus::Conflicted
            | NodeStatus::Obstructed
    ) || matches!(s.props, PropStatus::Modified | PropStatus::Conflicted)
        || s.tree_conflict
        || s.moved_from.is_some()
        || s.moved_to.is_some()
}

/// Removes the working copy rooted at `abs` when it has no local changes: its versioned files,
/// then its emptied directories and its administrative directory. Unversioned and ignored items
/// stay, and so does the whole working copy when anything in it (including an external nested
/// in it) is modified.
fn remove_wc(
    owner: &LockOwner,
    abs: &Path,
    globals: Option<&[String]>,
    cancel: &CancelToken,
) -> Result<Removal> {
    match abs.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Removal::Removed),
        Err(e) => {
            return Err(SvnError::from_io(
                format!("cannot stat '{}'", abs.display()),
                &e,
            ));
        }
        Ok(m) if !m.is_dir() => {
            return Ok(Removal::Left("it is not a directory".to_owned()));
        }
        Ok(_) => {}
    }
    if !is_wc_root_dir(abs) {
        return Ok(if std::fs::remove_dir(abs).is_ok() {
            Removal::Removed
        } else {
            Removal::Left("it is not a working copy".to_owned())
        });
    }
    let (db, _) = WcDb::open_and_recover(abs, owner.clone())?;
    let db = Arc::new(db);
    let root = RelPath::root();
    {
        db.wc_lock_obtain(&root, -1, ObtainMode::Cleanup)?;
        let guard = RootLock(Arc::clone(&db));
        wc::run_work_queue(&db, cancel)?;
        // Externals nested in this one go first, under the same rules.
        for nested in db.externals_under(&root)? {
            cancel.check()?;
            if matches!(nested.kind, NodeKind::File | NodeKind::Symlink) {
                continue;
            }
            let nested_abs = wc::abs_path(&db, &nested.local_relpath)?;
            if let Removal::Left(why) = remove_wc(owner, &nested_abs, globals, cancel)? {
                return Ok(Removal::Left(format!(
                    "the external '{}' inside it was kept: {why}",
                    nested.local_relpath
                )));
            }
            db.with_txn(|t| t.external_remove(&nested.local_relpath))?;
        }
        let opts = StatusOptions {
            depth: Depth::Infinity,
            show_unmodified: false,
            include_unversioned: true,
            include_ignored: true,
            include_needs_lock: false,
        };
        let statuses = local_status(&db, &root, &opts, globals, cancel)?;
        if let Some(s) = statuses.iter().find(|s| is_local_change(s)) {
            return Ok(Removal::Left(format!(
                "'{}' has local modifications",
                s.path
            )));
        }
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        collect(&db, &root, &mut files, &mut dirs)?;
        // Each file is checked again right before it goes: one edited since the status ran
        // stays, and so does the working copy (with its administrative data) around it.
        let mut edited = None;
        for (rel, node) in files {
            cancel.check()?;
            let path = wc::abs_path(&db, &rel)?;
            match path.symlink_metadata() {
                Ok(m) if !m.is_dir() => {
                    if inspect::text_modified(&db, &rel, &node)? {
                        edited.get_or_insert(rel);
                        continue;
                    }
                    std::fs::remove_file(&path).map_err(|e| {
                        SvnError::from_io(format!("cannot remove '{}'", path.display()), &e)
                    })?;
                }
                _ => {}
            }
        }
        if let Some(rel) = edited {
            return Ok(Removal::Left(format!("'{rel}' has local modifications")));
        }
        // Deepest first; a directory that still holds unversioned items stays.
        for rel in dirs.iter().rev() {
            let _ = std::fs::remove_dir(wc::abs_path(&db, rel)?);
        }
        drop(guard);
    }
    drop(db);
    let adm = abs.join(svn_wc::db::ADM_DIR);
    std::fs::remove_dir_all(&adm)
        .map_err(|e| SvnError::from_io(format!("cannot remove '{}'", adm.display()), &e))?;
    if std::fs::remove_dir(abs).is_ok() {
        Ok(Removal::Removed)
    } else {
        Ok(Removal::Left(
            "unversioned items were left in place".to_owned(),
        ))
    }
}

/// Releases the subtree lock of a working copy being removed.
struct RootLock(Arc<WcDb>);

impl Drop for RootLock {
    fn drop(&mut self) {
        let _ = self.0.wc_lock_release(&RelPath::root());
    }
}

/// Present BASE files (and symlinks) and directories below `dir`, parents before children.
fn collect(
    db: &WcDb,
    dir: &RelPath,
    files: &mut Vec<(RelPath, Node)>,
    dirs: &mut Vec<RelPath>,
) -> Result<()> {
    for child in db.children(dir)? {
        let Some(node) = db.read_base(&child)? else {
            continue;
        };
        if !node.presence.is_present() {
            continue;
        }
        if node.kind == NodeKind::Dir {
            dirs.push(child.clone());
            collect(db, &child, files, dirs)?;
        } else {
            files.push((child, node));
        }
    }
    Ok(())
}

/// Removes the file external `ext` from `db`: its BASE row becomes a `not-present` marker as in
/// svn, and the file goes from disk unless it has local changes (then it stays, unversioned).
fn remove_file_external(
    db: &Arc<WcDb>,
    ext: &External,
    cancel: &CancelToken,
) -> Result<ExternalReport> {
    let local = &ext.local_relpath;
    let anchor = local.parent().unwrap_or_else(RelPath::root);
    let _lock = lock_nearest(db, &anchor, cancel)?;
    let node = db.read_base(local)?;
    let Some(node) = node.filter(|n| n.file_external && n.presence.is_present()) else {
        db.with_txn(|t| t.external_remove(local))?;
        return Ok(report(local, NodeKind::File, ExternalAction::Removed, None));
    };
    if db.conflict(local)?.is_some() {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("the file external '{local}' is in conflict; resolve the conflict first"),
        ));
    }
    let abs = wc::abs_path(db, local)?;
    let on_disk = abs.symlink_metadata().is_ok();
    let shadowed = db.read_node(local)?.is_some_and(|n| n.op_depth > 0);
    let modified = shadowed
        || inspect::props_modified(db, local)?
        || (on_disk && inspect::text_modified(db, local, &node)?);
    let marker = match db.read_base(&anchor)?.and_then(|p| p.repos_relpath) {
        Some(parent_repos) => {
            let (root, uuid) = wc::repos_identity(db)?;
            let name = local.file_name().unwrap_or_default();
            Some(BaseNode {
                kind: NodeKind::File,
                presence: Presence::NotPresent,
                repos_root_url: root,
                repos_uuid: uuid,
                repos_relpath: if parent_repos.is_empty() {
                    name.to_owned()
                } else {
                    format!("{parent_repos}/{name}")
                },
                revision: 0,
                changed_rev: None,
                changed_date: None,
                changed_author: None,
                depth: None,
                properties: None,
                checksum: None,
                symlink_target: None,
                translated_size: None,
                last_mod_time: None,
                inherited_props: None,
                dav_cache: None,
            })
        }
        None => None,
    };
    let work = if on_disk && !modified {
        vec![
            WorkItem::FileRemove {
                relpath: local.as_str().to_owned(),
            }
            .to_skel(),
        ]
    } else {
        Vec::new()
    };
    db.with_txn(|t| {
        t.base_remove(local)?;
        if let Some(m) = &marker {
            t.base_add(local, m)?;
        }
        t.external_remove(local)?;
        t.queue_work(&work)
    })?;
    wc::run_work_queue(db, cancel)?;
    Ok(if modified {
        ExternalReport {
            message: Some(format!(
                "the file external '{local}' has local modifications; it was left in place as \
                 an unversioned file"
            )),
            ..report(
                local,
                NodeKind::File,
                ExternalAction::LeftLocalModifications,
                None,
            )
        }
    } else {
        report(local, NodeKind::File, ExternalAction::Removed, None)
    })
}
