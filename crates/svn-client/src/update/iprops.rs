//! The inherited-properties cache of the working copy (`NODES.inherited_props`).
//!
//! svn caches the properties a node inherits from parents outside the working copy on the nodes
//! whose parents are not in it: the working copy root and every switched node (and file
//! externals, see [`crate::externals`]). svn 1.14 and TortoiseSVN read `svn:global-ignores`,
//! `svn:auto-props` and the other inheritable properties of the repository from there, so a
//! working copy without the cache loses them until svn updates it.
//!
//! Like `svn_client__get_inheritable_props` with svn's update editor: the nodes that had a cache
//! before the drive, plus the target when it is the root or switched, get the inherited
//! properties of their new repository location at the revision the drive reached; a node that is
//! neither the root nor switched any more loses its cache.

use svn_ra_api::{InheritedItem, RaSession};
use svn_types::{CancelToken, Depth, ErrorCode, RelPath, Result, Revnum};
use svn_wc::db::{Presence, WcDb};
use svn_wc::skel::InheritedProps;

/// Whether the BASE node at `rel` caches inherited properties: it is the working copy root or
/// switched (its repository path is not its parent's plus its name).
pub(crate) fn needs_cache(db: &WcDb, rel: &RelPath) -> Result<bool> {
    let Some(node) = db.read_base(rel)? else {
        return Ok(false);
    };
    if !matches!(node.presence, Presence::Normal | Presence::Incomplete) {
        return Ok(false);
    }
    let Some(repos_relpath) = node.repos_relpath else {
        return Ok(false);
    };
    let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
        return Ok(true);
    };
    let parent_path = db.read_base(&parent)?.and_then(|n| n.repos_relpath);
    Ok(match parent_path {
        Some(p) if p.is_empty() => repos_relpath != name,
        Some(p) => repos_relpath != format!("{p}/{name}"),
        None => true,
    })
}

/// Nodes at or below `target` (within `depth`) whose cache a drive of `target` refreshes:
/// those cached now, and `target` itself when it is the root or switched.
pub(crate) fn targets(db: &WcDb, target: &RelPath, depth: Depth) -> Result<Vec<RelPath>> {
    let levels = target.components().count();
    let mut out: Vec<RelPath> = db
        .iprop_cached_below(target)?
        .into_iter()
        .filter(|p| {
            let below = p.components().count() - levels;
            match depth {
                Depth::Empty => below == 0,
                Depth::Files | Depth::Immediates => below <= 1,
                _ => true,
            }
        })
        .collect();
    if !out.contains(target) && needs_cache(db, target)? {
        out.insert(0, target.clone());
    }
    Ok(out)
}

/// The cache value of a server answer.
pub(crate) fn to_cache(items: Vec<InheritedItem>) -> InheritedProps {
    items
        .into_iter()
        .map(|i| {
            (
                i.repos_relpath.into_bytes(),
                i.props
                    .into_iter()
                    .map(|(k, v)| (k.into_bytes(), v))
                    .collect(),
            )
        })
        .collect()
}

/// Rewrites the cache of `paths` for revision `rev` (see the module docs). A server that cannot
/// report inherited properties leaves the cache as it is.
pub(crate) async fn refresh(
    session: &dyn RaSession,
    db: &WcDb,
    paths: &[RelPath],
    rev: Revnum,
    cancel: &CancelToken,
) -> Result<()> {
    for rel in paths {
        cancel.check()?;
        let repos_relpath = db.read_base(rel)?.and_then(|n| n.repos_relpath);
        let keep = needs_cache(db, rel)?;
        let iprops = match repos_relpath {
            // The repository root inherits nothing; svn stores no cache there.
            Some(p) if keep && !p.is_empty() => {
                match session
                    .inherited_props(&RelPath::new(&p)?, rev, cancel)
                    .await
                {
                    Ok(items) => Some(to_cache(items)),
                    Err(e) if e.code == ErrorCode::ServerUnsupported => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
            _ => None,
        };
        db.set_base_inherited_props(rel, iprops.as_ref())?;
    }
    Ok(())
}
