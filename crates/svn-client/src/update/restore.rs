//! Restoring versioned items that vanished from disk, which `svn update` does before it reports.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;

use svn_types::{Depth, NodeKind, RelPath, Result, SvnError};
use svn_wc::db::{Node, Presence, WcDb};
use svn_wc::skel::Skel;
use svn_wc::workqueue::WorkItem;

use crate::wc::abs_path;

/// Names present on disk in directory `rel` (any type), or `None` when the directory itself is
/// missing. One `readdir` per directory instead of one `stat` per child, like svn's crawler.
fn listing(db: &WcDb, rel: &RelPath) -> Result<Option<HashSet<OsString>>> {
    let abs = abs_path(db, rel)?;
    match fs::read_dir(&abs) {
        Ok(entries) => {
            let mut names = HashSet::new();
            for e in entries {
                let e = e.map_err(|e| SvnError::from_io(format!("cannot read '{rel}'"), &e))?;
                names.insert(e.file_name());
            }
            Ok(Some(names))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SvnError::from_io(format!("cannot read '{rel}'"), &e)),
    }
}

/// Whether `node` exists on disk. A name found in its parent's listing (when one is given) is
/// there; anything else is checked with a `stat`, which also matches names the filesystem
/// folds (case, Unicode normalization) exactly as before.
fn on_disk(db: &WcDb, node: &Node, parent: Option<&Option<HashSet<OsString>>>) -> Result<bool> {
    let rel = &node.relpath;
    if let Some(listing) = parent {
        let name = rel.file_name().unwrap_or("");
        match listing {
            Some(names) if names.contains(OsStr::new(name)) => return Ok(true),
            // The parent directory is missing: so is everything below it.
            None => return Ok(false),
            Some(_) => {}
        }
    }
    match fs::symlink_metadata(abs_path(db, rel)?) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
    }
}

fn walk(
    db: &WcDb,
    node: &Node,
    parent: Option<&Option<HashSet<OsString>>>,
    depth: Depth,
    use_commit_times: bool,
    items: &mut Vec<Skel>,
    restored: &mut Vec<(RelPath, NodeKind)>,
) -> Result<()> {
    let rel = &node.relpath;
    // Only plain BASE nodes: scheduled deletes, additions and copies are the user's business.
    if node.op_depth != 0 || node.presence != Presence::Normal {
        return Ok(());
    }
    let missing = !on_disk(db, node, parent)?;
    match node.kind {
        NodeKind::File | NodeKind::Symlink => {
            if missing {
                items.push(
                    WorkItem::FileInstall {
                        relpath: rel.as_str().to_owned(),
                        use_commit_times,
                        record_fileinfo: true,
                        source: None,
                    }
                    .to_skel(),
                );
                restored.push((rel.clone(), node.kind));
            }
        }
        NodeKind::Dir => {
            if missing {
                items.push(
                    WorkItem::DirInstall {
                        relpath: rel.as_str().to_owned(),
                    }
                    .to_skel(),
                );
                restored.push((rel.clone(), NodeKind::Dir));
            }
            // One query and one directory listing per directory instead of one of each per
            // child (a missing directory has nothing below it on disk).
            let names = if missing {
                Some(None)
            } else {
                match fs::symlink_metadata(abs_path(db, rel)?) {
                    Ok(m) if m.file_type().is_dir() => Some(listing(db, rel)?),
                    // A directory replaced by a file or a symlink (obstructed) is left alone:
                    // nothing can be restored below it, and a symlink is never followed.
                    Ok(_) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
                    Err(e) => return Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
                }
            };
            // Like svn's crawler: the children of a crawled directory are restored, and the
            // crawl only descends further for a recursive depth.
            if matches!(depth, Depth::Empty | Depth::Exclude) {
                return Ok(());
            }
            let below = match depth {
                Depth::Files | Depth::Immediates => Depth::Empty,
                d => d,
            };
            for child in db.current_children(rel)? {
                walk(
                    db,
                    &child,
                    names.as_ref(),
                    below,
                    use_commit_times,
                    items,
                    restored,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Queues the re-creation of every BASE file or directory within `depth` of `root` (`Unknown`:
/// all of it) that is missing on disk and returns what was queued. The caller runs the work
/// queue.
pub fn queue_missing(
    db: &WcDb,
    root: &RelPath,
    depth: Depth,
    use_commit_times: bool,
) -> Result<Vec<(RelPath, NodeKind)>> {
    let mut items = Vec::new();
    let mut restored = Vec::new();
    if let Some(node) = db.read_node(root)? {
        walk(
            db,
            &node,
            None,
            depth,
            use_commit_times,
            &mut items,
            &mut restored,
        )?;
    }
    if !items.is_empty() {
        db.queue_work(&items)?;
    }
    Ok(restored)
}
