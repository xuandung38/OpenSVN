//! Harvesting committable changes from the working copy (read-only).

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use svn_types::{Depth, ErrorCode, NodeKind, PropChange, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::pristine::Sha1Digest;

use crate::local::delete::{cannot_delete_external, external_definer};
use crate::local::paths::{abs_of, depth_of};
use crate::local::state::{NodeInfo, Schedule, node_info, props_modified, text_modified};

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

/// A repository lock held by this working copy on a path met by the harvest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestLock {
    /// Working copy relative path.
    pub relpath: RelPath,
    /// Repository relative path the lock is on.
    pub repos_relpath: String,
    /// Lock token.
    pub token: String,
}

/// Result of harvesting.
#[derive(Debug, Clone, Default)]
pub struct Harvest {
    /// Committables sorted by repository path.
    pub items: Vec<Committable>,
    /// Every lock token met by the walk (`harvest_status_callback`): those of committed paths,
    /// of unmodified locked files below the targets, and of every locked node below a deleted or
    /// replaced directory. All are sent with the commit (the server needs the ones below a
    /// deleted directory) and released afterwards unless the locks are kept.
    pub locks: Vec<HarvestLock>,
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
    locks: BTreeMap<RelPath, HarvestLock>,
    /// Paths below the targets with an `ACTUAL_NODE` row: only these can have local property
    /// changes, a conflict or a changelist, so the others skip those queries.
    actual: BTreeSet<RelPath>,
    /// Paths below the targets whose BASE node has a lock token in the working copy.
    locked: BTreeSet<RelPath>,
}

/// Harvests the changes below `targets`.
pub fn collect(db: &WcDb, targets: &[RelPath], opts: &CollectOptions) -> Result<Harvest> {
    // Tens of thousands of reads: one read transaction instead of one per statement.
    db.read_txn(|| collect_inner(db, targets, opts))
}

fn collect_inner(db: &WcDb, targets: &[RelPath], opts: &CollectOptions) -> Result<Harvest> {
    let mut c = Collector {
        db,
        opts,
        items: BTreeMap::new(),
        locks: BTreeMap::new(),
        actual: BTreeSet::new(),
        locked: BTreeSet::new(),
    };
    for t in targets {
        c.actual.extend(db.actual_rows_below(t)?);
        c.locked
            .extend(db.repos_locks_below(t)?.into_iter().map(|(rel, _, _)| rel));
    }
    for t in targets {
        // An explicit target must be versioned.
        node_info(db, t)?;
        c.walk(t, opts.depth, true)?;
    }
    c.check_parents()?;
    c.check_moves()?;
    let mut items: Vec<Committable> = c.items.into_values().collect();
    items.sort_by(|a, b| a.repos_relpath.cmp(&b.repos_relpath));
    let mut dir_revisions = BTreeMap::new();
    let mut seen_dirs = BTreeSet::new();
    for it in &items {
        let mut cur = it.relpath.parent();
        while let Some(d) = cur {
            // Its ancestors were looked at with it.
            if !seen_dirs.insert(d.clone()) {
                break;
            }
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
        locks: c.locks.into_values().collect(),
    })
}

impl Collector<'_> {
    fn walk(&mut self, rel: &RelPath, depth: Depth, explicit: bool) -> Result<()> {
        let info = match node_info(self.db, rel) {
            Ok(i) => i,
            Err(e) if e.code == ErrorCode::PathNotFound && !explicit => return Ok(()),
            Err(e) => return Err(e),
        };
        // A file external is committed only when it is itself a commit target, as
        // libsvn_client's harvest does: its BASE location is the external's source, which a
        // commit of an enclosing folder must never change. (A directory external is a separate
        // working copy with no rows here, so it is never walked.)
        if !explicit && info.base.as_ref().is_some_and(|b| b.file_external) {
            return Ok(());
        }
        if self.actual.contains(rel) && self.db.conflict(rel)?.is_some() {
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
        // Every child row: `walk` itself skips what is not live (not versioned, not-present,
        // excluded), like `live_children` would, without reading each node twice.
        for child in self.db.children(rel)? {
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
            let member = self.actual.contains(rel)
                && db
                    .changelist(rel)?
                    .is_some_and(|c| self.opts.changelists.contains(&c));
            if !member {
                return Ok(());
            }
        }
        // An external is never deleted (nor replaced) by a commit: its BASE location is the
        // external's source, so the commit would delete that from the repository. `delete`
        // refuses to schedule it; this guards rows written by anything else.
        if info.op_root
            && matches!(info.schedule, Schedule::Deleted | Schedule::Replaced)
            && let Some(def) = external_definer(db, rel)?
        {
            return Err(cannot_delete_external(db, rel, &def));
        }
        self.gather_locks(info)?;
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
                if !text && !self.props_modified(rel)? {
                    return Ok(());
                }
                (ItemAction::Modify, None, None, text)
            }
            Schedule::Normal => {
                let text = kind == ItemKind::File && self.file_needs_text(info, false)?;
                if !text && !self.props_modified(rel)? {
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
        let lock_token = if info.schedule == Schedule::Added || !self.locked.contains(rel) {
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

    /// Records the lock tokens of `info`: its own BASE lock, or for the root of a deletion or
    /// replacement (which deletes the BASE node first) every lock at or below it, like svn's
    /// `harvest_status_callback` for any item with the delete flag.
    fn gather_locks(&mut self, info: &NodeInfo) -> Result<()> {
        let found =
            if matches!(info.schedule, Schedule::Deleted | Schedule::Replaced) && info.op_root {
                self.db.repos_locks_below(&info.rel)?
            } else if info.schedule == Schedule::Deleted {
                Vec::new() // gathered with its deletion root
            } else {
                let lock = if self.locked.contains(&info.rel) {
                    self.db.repos_lock(&info.rel)?
                } else {
                    None
                };
                match (&info.base, lock) {
                    (Some(b), Some(l)) => b
                        .repos_relpath
                        .clone()
                        .map(|r| vec![(info.rel.clone(), r, l)])
                        .unwrap_or_default(),
                    _ => Vec::new(),
                }
            };
        for (rel, repos_relpath, lock) in found {
            self.locks.insert(
                rel.clone(),
                HarvestLock {
                    relpath: rel,
                    repos_relpath,
                    token: lock.token,
                },
            );
        }
        Ok(())
    }

    /// `rel` or one of its ancestors is committed (a move source inside a deleted directory is
    /// committed with that deletion).
    /// [`props_modified`], answered without a query for a node without an `ACTUAL_NODE` row
    /// (its working properties are its pristine ones).
    fn props_modified(&self, rel: &RelPath) -> Result<bool> {
        Ok(self.actual.contains(rel) && props_modified(self.db, rel)?)
    }

    fn covers(&self, rel: &RelPath, ancestor_action: ItemAction) -> bool {
        if self.items.contains_key(rel) {
            return true;
        }
        let mut cur = rel.parent();
        while let Some(p) = cur {
            if self
                .items
                .get(&p)
                .is_some_and(|i| i.action == ancestor_action || i.action == ItemAction::Replace)
            {
                return true;
            }
            cur = p.parent();
        }
        false
    }

    /// Both halves of a local move must be committed together (svn's E200009).
    fn check_moves(&self) -> Result<()> {
        let illegal =
            |msg: String| SvnError::new(ErrorCode::WcInvalidOperation, msg).with_svn_code(200_009);
        for (rel, item) in &self.items {
            if matches!(item.action, ItemAction::Add | ItemAction::Replace)
                && self
                    .db
                    .read_node(rel)?
                    .is_some_and(|n| n.moved_here && n.op_depth == depth_of(rel))
                && let Some(src) = self.db.moved_from(rel)?
                && !self.covers(&src, ItemAction::Delete)
            {
                return Err(illegal(format!(
                    "Cannot commit '{rel}' because it was moved from '{src}' which is not part \
                     of the commit; both sides of the move must be committed together"
                )));
            }
            if matches!(item.action, ItemAction::Delete | ItemAction::Replace)
                && let Some(dst) = self.db.moved_to(rel)?
                && !self.covers(&dst, ItemAction::Add)
            {
                return Err(illegal(format!(
                    "Cannot commit '{rel}' because it was moved to '{dst}' which is not part \
                     of the commit; both sides of the move must be committed together"
                )));
            }
        }
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

    /// Every added path whose parent is added too needs that parent in the commit, and so does
    /// a change below a copy (the server does not know the copied path yet).
    fn check_parents(&self) -> Result<()> {
        for (rel, item) in &self.items {
            let in_working_layer = item.action == ItemAction::Modify
                && self.db.read_node(rel)?.is_some_and(|n| n.op_depth != 0);
            if !matches!(item.action, ItemAction::Add | ItemAction::Replace) && !in_working_layer {
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
                            "'{p}' is not known to exist in the repository and is not part of \
                             the commit, yet its child '{rel}' is part of the commit"
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
