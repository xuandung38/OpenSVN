//! Restoring versioned items that vanished from disk, which `svn update` does before it reports.

use std::fs;

use svn_types::{Depth, NodeKind, RelPath, Result, SvnError};
use svn_wc::db::{Presence, WcDb};
use svn_wc::skel::Skel;
use svn_wc::workqueue::WorkItem;

use crate::wc::abs_path;

fn walk(
    db: &WcDb,
    rel: &RelPath,
    depth: Depth,
    use_commit_times: bool,
    items: &mut Vec<Skel>,
    restored: &mut Vec<(RelPath, NodeKind)>,
) -> Result<()> {
    let Some(node) = db.read_node(rel)? else {
        return Ok(());
    };
    // Only plain BASE nodes: scheduled deletes, additions and copies are the user's business.
    if node.op_depth != 0 || node.presence != Presence::Normal {
        return Ok(());
    }
    let (missing, real_dir) = match fs::symlink_metadata(abs_path(db, rel)?) {
        Ok(md) => (false, md.file_type().is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (true, false),
        Err(e) => return Err(SvnError::from_io(format!("cannot stat '{rel}'"), &e)),
    };
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
            // A directory replaced by a file or a symlink (obstructed) is left alone: nothing
            // can be restored below it, and a symlink is never followed.
            if !missing && !real_dir {
                return Ok(());
            }
            // Like svn's crawler: the children of a crawled directory are restored, and the
            // crawl only descends further for a recursive depth.
            if matches!(depth, Depth::Empty | Depth::Exclude) {
                return Ok(());
            }
            let below = match depth {
                Depth::Files | Depth::Immediates => Depth::Empty,
                d => d,
            };
            for child in db.children(rel)? {
                walk(db, &child, below, use_commit_times, items, restored)?;
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
    walk(db, root, depth, use_commit_times, &mut items, &mut restored)?;
    if !items.is_empty() {
        db.queue_work(&items)?;
    }
    Ok(restored)
}
