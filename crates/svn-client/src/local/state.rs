//! Local state of versioned nodes: schedule, modification checks and tree walking.

use svn_types::{ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::db::{Node, Presence};
use svn_wc::pristine::Sha1Digest;

use super::content::{detranslate_digest, stat_cache};
use super::paths::{abs_of, depth_of};

/// What the user has scheduled for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Unchanged structure (text and properties may still be modified).
    Normal,
    /// Added or copied here.
    Added,
    /// Deleted (the node or a parent).
    Deleted,
    /// Deleted and added back.
    Replaced,
}

/// Current state of one versioned node.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Working copy relative path.
    pub rel: RelPath,
    /// Highest layer row.
    pub node: Node,
    /// BASE row, if any.
    pub base: Option<Node>,
    /// Schedule derived from the layers.
    pub schedule: Schedule,
    /// The node is the root of its add/copy/delete operation (its `op_depth` equals its depth).
    pub op_root: bool,
}

fn not_versioned(rel: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("'{rel}' is not under version control"),
    )
}

/// Reads the state of `rel`; `PathNotFound` when it is not versioned (not-present and excluded
/// BASE rows count as not versioned).
pub fn node_info(db: &WcDb, rel: &RelPath) -> Result<NodeInfo> {
    let node = db.read_node(rel)?.ok_or_else(|| not_versioned(rel))?;
    let base = db.read_base(rel)?;
    let base_present = base.as_ref().is_some_and(|b| b.presence.is_present());
    let schedule = match node.presence {
        Presence::BaseDeleted => Schedule::Deleted,
        Presence::Normal | Presence::Incomplete if node.op_depth == 0 => Schedule::Normal,
        Presence::Normal | Presence::Incomplete => {
            if base_present && node.op_depth == depth_of(rel) {
                Schedule::Replaced
            } else {
                Schedule::Added
            }
        }
        _ => return Err(not_versioned(rel)),
    };
    let op_root = node.op_depth != 0 && node.op_depth == depth_of(rel);
    Ok(NodeInfo {
        rel: rel.clone(),
        node,
        base,
        schedule,
        op_root,
    })
}

/// True when `rel` is a live versioned node.
pub fn is_versioned(db: &WcDb, rel: &RelPath) -> Result<bool> {
    Ok(match node_info(db, rel) {
        Ok(_) => true,
        Err(e) if e.code == ErrorCode::PathNotFound => false,
        Err(e) => return Err(e),
    })
}

/// True when the working properties differ from the pristine ones.
pub fn props_modified(db: &WcDb, rel: &RelPath) -> Result<bool> {
    let mut a = db.actual_props(rel)?;
    let mut p = db.pristine_props(rel)?;
    a.sort();
    p.sort();
    Ok(a != p)
}

/// True when the working file differs from the pristine text of the node (an added file always
/// counts as modified). A missing file is reported as not modified.
pub fn text_modified(db: &WcDb, info: &NodeInfo) -> Result<bool> {
    if info.node.kind != NodeKind::File {
        return Ok(false);
    }
    let abs = abs_of(db, &info.rel);
    let Some((size, mtime)) = stat_cache(&abs) else {
        return Ok(false);
    };
    let Some(expected) = info
        .node
        .checksum
        .as_deref()
        .and_then(Sha1Digest::from_svn_string)
    else {
        return Ok(true);
    };
    if info.node.translated_size == Some(size) && info.node.last_mod_time == Some(mtime) {
        return Ok(false);
    }
    Ok(detranslate_digest(db, &info.rel, &abs)?.sha1 != expected)
}

/// True when a conflict is recorded on `rel`.
pub fn has_conflict(db: &WcDb, rel: &RelPath) -> Result<bool> {
    Ok(db.conflict(rel)?.is_some())
}

/// Live children of `rel` (present, or scheduled for deletion), sorted.
pub fn live_children(db: &WcDb, rel: &RelPath) -> Result<Vec<RelPath>> {
    let mut out = Vec::new();
    for c in db.children(rel)? {
        if let Some(n) = db.read_node(&c)?
            && matches!(
                n.presence,
                Presence::Normal | Presence::Incomplete | Presence::BaseDeleted
            )
        {
            out.push(c);
        }
    }
    Ok(out)
}

/// `rel` followed by every live descendant, parents before children.
pub fn subtree(db: &WcDb, rel: &RelPath) -> Result<Vec<RelPath>> {
    let mut out = vec![rel.clone()];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i].clone();
        i += 1;
        out.extend(live_children(db, &cur)?);
    }
    Ok(out)
}
