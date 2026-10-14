//! `svn update --set-depth`: changing the recorded depth of a working copy directory.
//!
//! Shrinking crops first, the way svn 1.15 does (`libsvn_wc/crop.c`): a child outside the new
//! depth leaves the working copy only when nothing below it is locally modified (deletes aside).
//! A child with local changes stays versioned, and a directory among them is cropped to depth
//! `empty` instead, so modified files are never removed from disk. Then, like svn, the target is
//! updated with the new depth made sticky, which also brings in what a deeper depth adds.

use std::path::Path;
use std::sync::Arc;

use svn_ra_api::RaSession;
use svn_types::{
    CancelToken, Depth, ErrorCode, NodeKind, NodeStatus, PropStatus, RelPath, Result, Revnum,
    SvnError,
};
use svn_wc::db::{Presence, WcDb};

use crate::ctx::{ClientCtx, UpdateAction, UpdateEvent, UpdateSummary};
use crate::local::lock::write_op;
use crate::local::paths::abs_of;
use crate::status::{StatusOptions, local_status};
use crate::update::{UpdateKind, UpdateParams, drive_update};
use crate::wc;

fn unsupported(msg: String) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

fn rank(d: Depth) -> u8 {
    match d {
        Depth::Exclude => 0,
        Depth::Empty => 1,
        Depth::Files => 2,
        Depth::Immediates => 3,
        Depth::Infinity | Depth::Unknown => 4,
    }
}

/// `svn_wc__node_has_local_mods` with unversioned items counted: `(modified, all_deletes)`.
/// Ignored items and externals are no modification; a delete with something on disk in its
/// place is (the delete is obstructed by the user's data).
fn local_mods(db: &WcDb, rel: &RelPath, cancel: &CancelToken) -> Result<(bool, bool)> {
    let opts = StatusOptions {
        depth: Depth::Infinity,
        show_unmodified: false,
        include_unversioned: true,
        include_ignored: false,
    };
    let mut modified = false;
    for st in local_status(db, rel, &opts, None, cancel)? {
        let props_changed = matches!(st.props, PropStatus::Modified | PropStatus::Conflicted);
        let not_delete = match st.text {
            NodeStatus::Normal
            | NodeStatus::None
            | NodeStatus::Ignored
            | NodeStatus::External
            | NodeStatus::Excluded
            | NodeStatus::Incomplete => props_changed || st.tree_conflict,
            NodeStatus::Deleted => {
                modified = true;
                st.tree_conflict || abs_of(db, &st.path).symlink_metadata().is_ok()
            }
            _ => true,
        };
        if not_delete {
            return Ok((true, false));
        }
    }
    Ok((modified, modified))
}

/// Port of svn's `crop_children`: records `new_depth` on `dir` and removes or crops the children
/// it no longer covers. Removed nodes are appended to `removed`.
fn crop_children(
    db: &WcDb,
    cancel: &CancelToken,
    dir: &RelPath,
    dir_depth: Depth,
    new_depth: Depth,
    removed: &mut Vec<(RelPath, NodeKind)>,
) -> Result<()> {
    cancel.check()?;
    if rank(dir_depth) > rank(new_depth) {
        db.set_depth(dir, new_depth)?;
    }
    for child in db.children(dir)? {
        let Some(base) = db.read_base(&child)? else {
            continue;
        };
        let cur = db.read_node(&child)?.unwrap_or_else(|| base.clone());
        let kind = cur.kind;
        if cur.op_depth > 0 {
            if cur.presence != Presence::BaseDeleted {
                continue; // local additions stay
            }
            let (modified, all_deletes) = local_mods(db, &child, cancel)?;
            if modified && !all_deletes {
                continue;
            }
        }
        let remove_below = if kind == NodeKind::Dir {
            Depth::Immediates
        } else {
            Depth::Files
        };
        if cur.op_depth == 0
            && matches!(
                base.presence,
                Presence::NotPresent | Presence::Excluded | Presence::ServerExcluded
            )
        {
            if rank(new_depth) < rank(remove_below) {
                db.sparse_remove(&child, false, &[])?;
            }
            continue;
        }
        if rank(new_depth) < rank(remove_below) {
            let (modified, all_deletes) = local_mods(db, &child, cancel)?;
            if !modified || all_deletes {
                db.sparse_remove(&child, false, &[])?;
                removed.push((child, kind));
                continue;
            }
        }
        if kind == NodeKind::Dir {
            let depth = base.depth.unwrap_or(Depth::Infinity);
            crop_children(db, cancel, &child, depth, Depth::Empty, removed)?;
        }
    }
    Ok(())
}

/// The BASE directory `rel` must be a present, unscheduled directory to change its depth.
fn check_target(db: &WcDb, rel: &RelPath, what: &str) -> Result<svn_wc::db::Node> {
    let base = db.read_base(rel)?;
    let cur = db.read_node(rel)?;
    if base.is_none() && cur.as_ref().is_some_and(|c| c.presence.is_present()) {
        return Err(unsupported(format!(
            "cannot {what} '{rel}': it is to be added to the repository; commit or revert first"
        )));
    }
    let (Some(base), Some(cur)) = (base, cur) else {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{rel}' is not under version control"),
        ));
    };
    if !base.presence.is_present() && base.presence != Presence::Excluded {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("the node '{rel}' was not found"),
        ));
    }
    if cur.op_depth > 0 {
        let why = if cur.presence == Presence::BaseDeleted {
            "it is going to be removed from the repository; commit or revert first"
        } else {
            "it is to be added to the repository; commit or revert first"
        };
        return Err(unsupported(format!("cannot {what} '{rel}': {why}")));
    }
    Ok(base)
}

/// Crops the directory `rel` to `depth` (`empty`, `files` or `immediates`), like
/// `svn_wc_crop_tree2`, under the working copy write lock. Returns the removed nodes.
pub fn crop(
    db: &WcDb,
    cancel: &CancelToken,
    rel: &RelPath,
    depth: Depth,
) -> Result<Vec<(RelPath, NodeKind)>> {
    if !matches!(depth, Depth::Empty | Depth::Files | Depth::Immediates) {
        return Err(unsupported(
            "can only crop a working copy with a restrictive depth".to_owned(),
        ));
    }
    write_op(db, cancel, || {
        let base = check_target(db, rel, "crop")?;
        if base.presence == Presence::Excluded {
            return Ok(Vec::new());
        }
        if base.kind != NodeKind::Dir {
            return Err(unsupported(format!(
                "can only crop directories, not '{rel}'"
            )));
        }
        let mut removed = Vec::new();
        let depth_now = base.depth.unwrap_or(Depth::Infinity);
        crop_children(db, cancel, rel, depth_now, depth, &mut removed)?;
        Ok(removed)
    })
}

/// `svn update --set-depth exclude`, like `svn_wc_exclude`: removes `rel` from the working copy
/// and records it as `excluded`, unless something below it is locally modified; then it is
/// cropped to depth `empty` instead. Returns the removed nodes.
pub fn exclude(db: &WcDb, cancel: &CancelToken, rel: &RelPath) -> Result<Vec<(RelPath, NodeKind)>> {
    write_op(db, cancel, || {
        let Some(parent) = rel.parent() else {
            return Err(unsupported(format!(
                "cannot exclude '{rel}': it is a working copy root"
            )));
        };
        let base = check_target(db, rel, "exclude")?;
        if base.presence == Presence::Excluded {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("the node '{rel}' was not found"),
            ));
        }
        let parent_repos = db.read_base(&parent)?.and_then(|p| p.repos_relpath);
        let name = rel.file_name().unwrap_or("");
        let expected = match parent_repos.as_deref() {
            Some("") => name.to_owned(),
            Some(p) => format!("{p}/{name}"),
            None => String::new(),
        };
        if base.repos_relpath.as_deref() != Some(expected.as_str()) {
            return Err(unsupported(format!(
                "cannot exclude '{rel}': it is a switched path"
            )));
        }
        let mut removed = Vec::new();
        let (modified, all_deletes) = local_mods(db, rel, cancel)?;
        if !modified || all_deletes {
            db.sparse_remove(rel, true, &[])?;
            removed.push((rel.clone(), base.kind));
        } else if base.kind == NodeKind::Dir {
            let depth = base.depth.unwrap_or(Depth::Infinity);
            crop_children(db, cancel, rel, depth, Depth::Empty, &mut removed)?;
        }
        Ok(removed)
    })
}

/// `svn update --set-depth DEPTH PATH`: changes the recorded depth of the working copy
/// directory at `path` and updates it to `revision` (HEAD when `None`) at that depth.
///
/// A narrower depth crops first (see the module documentation; locally modified items are kept
/// and reported by nothing being removed for them), a wider one fetches what is missing. `exclude`
/// removes the directory itself and records it as excluded, without contacting the server.
/// Only directories can change depth; a file can only be excluded.
pub async fn set_depth(
    ctx: &ClientCtx,
    session: &dyn RaSession,
    path: &Path,
    revision: Option<Revnum>,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<UpdateSummary> {
    if depth == Depth::Unknown {
        return Err(unsupported("a depth to set is required".to_owned()));
    }
    // A symlink would lead to whatever it points at, possibly another working copy whose
    // files cropping would remove.
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(unsupported(format!(
            "'{}' is a symbolic link; change the depth of the folder it points to instead",
            path.display()
        )));
    }
    session.ensure_authenticated(cancel).await?;
    let (owner, p) = (ctx.lock_owner.clone(), path.to_path_buf());
    let (db, rel) = tokio::task::spawn_blocking(move || wc::open_containing(&owner, &p))
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??;
    let db = Arc::new(db);
    let (db2, rel2, cancel2) = (Arc::clone(&db), rel.clone(), cancel.clone());
    let removed = tokio::task::spawn_blocking(move || -> Result<_> {
        let base = check_target(&db2, &rel2, "change the depth of")?;
        if base.presence == Presence::Excluded {
            return Err(unsupported(format!(
                "'{rel2}' is excluded from the working copy; change the depth of its parent \
                 folder to bring it back"
            )));
        }
        if base.kind != NodeKind::Dir && depth != Depth::Exclude {
            return Err(unsupported(format!(
                "only directories have a depth; '{rel2}' is a file"
            )));
        }
        match depth {
            Depth::Exclude => exclude(&db2, &cancel2, &rel2),
            Depth::Infinity => Ok(Vec::new()),
            _ => crop(&db2, &cancel2, &rel2, depth),
        }
    })
    .await
    .map_err(|e| SvnError::new(ErrorCode::Internal, format!("worker failed: {e}")))??;
    let events: Vec<UpdateEvent> = removed
        .into_iter()
        .map(|(path, kind)| UpdateEvent {
            path,
            kind,
            action: UpdateAction::Delete,
        })
        .collect();
    for e in &events {
        ctx.notify(e);
    }
    if depth == Depth::Exclude {
        return Ok(UpdateSummary {
            revision: None,
            events,
        });
    }
    let mut summary = drive_update(
        ctx,
        session,
        db,
        rel,
        UpdateParams {
            revision,
            depth,
            kind: UpdateKind::Update,
            allow_unversioned_obstructions: false,
            ignore_ancestry: false,
            sticky_depth: true,
        },
        cancel,
    )
    .await?;
    let mut all = events;
    all.append(&mut summary.events);
    summary.events = all;
    Ok(summary)
}
