//! Harvesting committable changes from the working copy (read-only).

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use svn_types::{Depth, ErrorCode, NodeKind, PropChange, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::pristine::Sha1Digest;

use crate::local::paths::{abs_of, depth_of};
use crate::local::state::{
    NodeInfo, Schedule, live_children, node_info, props_modified, text_modified,
};

/// What a committable does in the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemAction {
    /// New node (possibly copied from history).
    Add,
    /// Existing node with changed text and/or properties.
    Modify,
    /// Node removal.
    Delete,
    /// Delete followed by add at the same path.
    Replace,
}

/// File or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    /// Regular file (symlinks are files with `svn:special`).
    File,
    /// Directory.
    Dir,
}

/// Copy source of an added node, in repository terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySource {
    /// Repository relative path of the source.
    pub repos_relpath: String,
    /// Source revision.
    pub revision: u64,
}

/// One path of a commit.
#[derive(Debug, Clone)]
pub struct Committable {
    /// Working copy relative path.
    pub relpath: RelPath,
    /// Repository relative path the node has after the commit.
    pub repos_relpath: String,
    /// File or directory.
    pub kind: ItemKind,
    /// Operation.
    pub action: ItemAction,
    /// Copy source for `Add`/`Replace`.
    pub copy_from: Option<CopySource>,
    /// BASE revision the change is made against (`None` for nodes below an added tree).
    pub base_rev: Option<u64>,
    /// A new or changed text must be uploaded.
    pub needs_text: bool,
    /// Pristine SHA-1 of the node, the delta base for modified files.
    pub base_sha1: Option<Sha1Digest>,
    /// Property differences against the pristine properties (`None` value deletes).
    pub prop_changes: Vec<PropChange>,
    /// Lock token held for the path.
    pub lock_token: Option<String>,
}

/// Result of harvesting.
#[derive(Debug, Clone, Default)]
pub struct Harvest {
    /// Committables sorted by repository path.
    pub items: Vec<Committable>,
    /// BASE revision of every versioned ancestor directory of an item, by repository path, used
    /// to open the intermediate directories of the commit drive.
    pub dir_revisions: BTreeMap<String, u64>,
}

/// Parameters of [`collect`].
#[derive(Debug, Clone)]
pub struct CollectOptions {
    /// How deep below directory targets to look.
    pub depth: Depth,
    /// Only commit files in these changelists (empty = no filter).
    pub changelists: Vec<String>,
}

fn blocked(msg: String) -> SvnError {
    SvnError::new(ErrorCode::ConflictBlocksCommit, msg)
}

/// Repository path where `rel` lives after a commit: its BASE location, or the parent's plus
/// its name for anything added or copied.
pub fn destination_repos_relpath(db: &WcDb, rel: &RelPath) -> Result<String> {
    Ok(db.read_repos_info(rel)?.repos_relpath)
}

struct Collector<'a> {
    db: &'a WcDb,
    opts: &'a CollectOptions,
    items: BTreeMap<RelPath, Committable>,
}

/// Harvests the changes below `targets`.
pub fn collect(db: &WcDb, targets: &[RelPath], opts: &CollectOptions) -> Result<Harvest> {
    let mut c = Collector {
        db,
        opts,
        items: BTreeMap::new(),
    };
    for t in targets {
        // An explicit target must be versioned.
        node_info(db, t)?;
        c.walk(t, opts.depth, true)?;
    }
    c.check_parents()?;
    let mut items: Vec<Committable> = c.items.into_values().collect();
    items.sort_by(|a, b| a.repos_relpath.cmp(&b.repos_relpath));
    let mut dir_revisions = BTreeMap::new();
    for it in &items {
        let mut cur = it.relpath.parent();
        while let Some(d) = cur {
            cur = d.parent();
            if let Some(n) = db.read_node(&d)?
                && n.op_depth == 0
                && let (Some(p), Some(r)) = (n.repos_relpath, n.revision)
            {
                dir_revisions.insert(p, r);
            }
        }
    }
    Ok(Harvest {
        items,
        dir_revisions,
    })
}

impl Collector<'_> {
    fn walk(&mut self, rel: &RelPath, depth: Depth, explicit: bool) -> Result<()> {
        let info = match node_info(self.db, rel) {
            Ok(i) => i,
            Err(e) if e.code == ErrorCode::PathNotFound && !explicit => return Ok(()),
            Err(e) => return Err(e),
        };
        if self.db.conflict(rel)?.is_some() {
            return Err(blocked(format!(
                "aborting commit: '{rel}' remains in conflict"
            )));
        }
        self.visit(&info)?;
        if info.node.kind != NodeKind::Dir {
            return Ok(());
        }
        if info.schedule == Schedule::Deleted && info.op_root {
            return Ok(());
        }
        let (child_depth, files_only) = match depth {
            Depth::Empty | Depth::Exclude => return Ok(()),
            Depth::Files => (Depth::Empty, true),
            Depth::Immediates => (Depth::Empty, false),
            Depth::Infinity | Depth::Unknown => (Depth::Infinity, false),
        };
        for child in live_children(self.db, rel)? {
            if files_only
                && self
                    .db
                    .read_node(&child)?
                    .is_some_and(|n| n.kind == NodeKind::Dir)
            {
                continue;
            }
            self.walk(&child, child_depth, false)?;
        }
        Ok(())
    }

    fn visit(&mut self, info: &NodeInfo) -> Result<()> {
        let db = self.db;
        let rel = &info.rel;
        let kind = match info.node.kind {
            NodeKind::Dir => ItemKind::Dir,
            NodeKind::File | NodeKind::Symlink => ItemKind::File,
            _ => return Ok(()),
        };
        if !self.opts.changelists.is_empty() {
            let member = db
                .changelist(rel)?
                .is_some_and(|c| self.opts.changelists.contains(&c));
            if !member {
                return Ok(());
            }
        }
        let (action, copy_from, base_rev, needs_text) = match info.schedule {
            Schedule::Deleted => {
                if !info.op_root {
                    return Ok(());
                }
                let rev = info.base.as_ref().and_then(|b| b.revision);
                (ItemAction::Delete, None, rev, false)
            }
            Schedule::Added | Schedule::Replaced if info.op_root => {
                let copy = match (&info.node.repos_relpath, info.node.revision) {
                    (Some(p), Some(r)) => Some(CopySource {
                        repos_relpath: p.clone(),
                        revision: r,
                    }),
                    _ => None,
                };
                let text = if kind == ItemKind::File {
                    self.file_needs_text(info, copy.is_none())?
                } else {
                    false
                };
                let action = if info.schedule == Schedule::Replaced {
                    ItemAction::Replace
                } else {
                    ItemAction::Add
                };
                // A replacement deletes the BASE node first: that delete is made against it.
                let base_rev = info.base.as_ref().and_then(|b| b.revision);
                (action, copy, base_rev, text)
            }
            Schedule::Added | Schedule::Replaced => {
                // Below an added or copied root: only changes against the copy are sent.
                let text = kind == ItemKind::File && self.file_needs_text(info, false)?;
                if !text && !props_modified(db, rel)? {
                    return Ok(());
                }
                (ItemAction::Modify, None, None, text)
            }
            Schedule::Normal => {
                let text = kind == ItemKind::File && self.file_needs_text(info, false)?;
                if !text && !props_modified(db, rel)? {
                    return Ok(());
                }
                (ItemAction::Modify, None, info.node.revision, text)
            }
        };
        let prop_changes = if action == ItemAction::Delete {
            Vec::new()
        } else {
            prop_changes(
                db,
                rel,
                action == ItemAction::Replace && copy_from.is_none(),
            )?
        };
        let lock_token = if info.schedule == Schedule::Added {
            None
        } else {
            db.repos_lock(rel)?.map(|l| l.token)
        };
        let base_sha1 = info
            .node
            .checksum
            .as_deref()
            .and_then(Sha1Digest::from_svn_string);
        self.items.insert(
            rel.clone(),
            Committable {
                relpath: rel.clone(),
                repos_relpath: destination_repos_relpath(db, rel)?,
                kind,
                action,
                copy_from,
                base_rev,
                needs_text,
                base_sha1: if action == ItemAction::Modify {
                    base_sha1
                } else {
                    None
                },
                prop_changes,
                lock_token,
            },
        );
        Ok(())
    }

    /// An added file always needs its text; a copied or versioned one only when modified.
    fn file_needs_text(&self, info: &NodeInfo, plain_add: bool) -> Result<bool> {
        let abs = abs_of(self.db, &info.rel);
        if abs.symlink_metadata().is_err() {
            if info.schedule == Schedule::Normal {
                return Ok(false); // missing: nothing to send
            }
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{}' is scheduled for addition but is missing", info.rel),
            ));
        }
        if plain_add || info.node.checksum.is_none() {
            return Ok(true);
        }
        text_modified(self.db, info)
    }

    /// Every added path whose parent is added too needs that parent in the commit.
    fn check_parents(&self) -> Result<()> {
        for (rel, item) in &self.items {
            if !matches!(item.action, ItemAction::Add | ItemAction::Replace) {
                continue;
            }
            let mut cur = rel.parent();
            while let Some(p) = cur {
                cur = p.parent();
                if self.items.contains_key(&p) {
                    break;
                }
                let Some(n) = self.db.read_node(&p)? else {
                    break;
                };
                if n.op_depth == 0 {
                    break;
                }
                if n.op_depth == depth_of(&p) {
                    return Err(SvnError::new(
                        ErrorCode::WcInvalidOperation,
                        format!(
                            "'{p}' is not under version control in the repository and is not \
                             part of the commit, yet its child '{rel}' is part of the commit"
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Differences between the pristine and the working properties of `rel`. With `all_new` the
/// comparison base is empty (a replacement with no history starts without properties).
fn prop_changes(db: &WcDb, rel: &RelPath, all_new: bool) -> Result<Vec<PropChange>> {
    let working = db.actual_props(rel)?;
    let pristine = if all_new {
        Vec::new()
    } else {
        db.pristine_props(rel)?
    };
    let old: BTreeMap<&[u8], &[u8]> = pristine
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    let new: BTreeMap<&[u8], &[u8]> = working
        .iter()
        .map(|(k, v)| (k.as_slice(), v.as_slice()))
        .collect();
    let names: BTreeSet<&[u8]> = old.keys().chain(new.keys()).copied().collect();
    let mut out = Vec::new();
    for name in names {
        if old.get(name) != new.get(name) {
            out.push(PropChange {
                name: String::from_utf8_lossy(name).into_owned(),
                value: new.get(name).map(|v| Bytes::copy_from_slice(v)),
            });
        }
    }
    Ok(out)
}
