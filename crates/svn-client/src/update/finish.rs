//! Directory completion, deletion of nodes and the final revision bump of an update drive.

use svn_types::{DirId, NodeKind, RelPath, Result, SvnError};
use svn_wc::conflicts::markers::create_prop_marker;
use svn_wc::conflicts::{PropConflict, TreeAction, TreeReason};
use svn_wc::db::{BaseNode, Presence, WcDb};
use svn_wc::skel::Skel;
use svn_wc::workqueue::WorkItem;

use super::conflict_record::{self, NewConflict};
use super::editor::{UpdateEditor, failpoint};
use super::props::merge_props;
use super::state::DirState;
use crate::ctx::UpdateAction;
use crate::wc::inspect;

/// Work items that remove the versioned content below `rel` bottom-up. Directories are removed
/// without recursion, so unversioned files inside survive.
fn removal_items(db: &WcDb, rel: &RelPath, out: &mut Vec<Skel>) -> Result<()> {
    let Some(node) = db.read_base(rel)? else {
        return Ok(());
    };
    if node.kind == NodeKind::Dir {
        for child in db.children(rel)? {
            removal_items(db, &child, out)?;
        }
        out.push(
            WorkItem::DirRemove {
                relpath: rel.as_str().to_owned(),
                recursive: false,
            }
            .to_skel(),
        );
    } else if node.presence.is_present() {
        out.push(
            WorkItem::FileRemove {
                relpath: rel.as_str().to_owned(),
            }
            .to_skel(),
        );
    }
    Ok(())
}

impl UpdateEditor {
    pub(super) fn delete_node(&mut self, wc: &RelPath, _parent: DirId) -> Result<()> {
        let db = self.s.db.clone();
        let Some(base) = db.read_base(wc)? else {
            return Ok(());
        };
        // An update target deleted on the server stays as a `not-present` marker at the new
        // revision (svn's `deleting_target`), so the parent still knows the node is gone.
        let marker = if *wc == self.target_wc && !wc.is_root() {
            let repos = base.repos_relpath.clone().unwrap_or_default();
            Some(self.not_present_node(base.kind, &repos)?)
        } else {
            None
        };
        if !base.presence.is_present() {
            db.with_txn(|t| {
                t.base_remove(wc)?;
                if let Some(m) = &marker {
                    t.base_add(wc, m)?;
                }
                Ok(())
            })?;
            return Ok(());
        }
        let kind = base.kind;
        if db.conflict(wc)?.is_some() {
            self.skipped.push(wc.clone());
            self.emit(wc, kind, UpdateAction::Skipped);
            return Ok(());
        }
        let shadowing = db.read_node(wc)?.filter(|c| c.op_depth > 0);
        let reason = match &shadowing {
            Some(c) if c.presence == Presence::BaseDeleted => Some(TreeReason::Deleted),
            Some(_) => Some(TreeReason::Replaced),
            None if inspect::subtree_has_local_changes(&db, wc)? => Some(TreeReason::Edited),
            None => None,
        };
        if let Some(reason) = reason {
            // Only local edits are kept as a copy of the old BASE (svn's `keep_as_working`). A
            // node that is already deleted or replaced locally just loses its BASE; the working
            // rows and the disk stay as they are.
            let keep_as_working = reason == TreeReason::Edited;
            let repos = base.repos_relpath.clone().unwrap_or_default();
            let (old, new) = self.side(Some(&base), &repos, NodeKind::None);
            let info = conflict_record::operation_info(
                self.s.switch_relpath.is_some(),
                &self.s.repos_root,
                &self.s.uuid,
                old.as_ref(),
                new.as_ref(),
            );
            db.with_txn(|t| {
                if keep_as_working {
                    t.convert_base_to_copy(wc)?;
                } else {
                    t.base_remove(wc)?;
                }
                if let Some(m) = &marker {
                    t.base_add(wc, m)?;
                }
                conflict_record::record(
                    t,
                    wc,
                    &info,
                    vec![NewConflict::Tree(reason.clone(), TreeAction::Delete)],
                )
            })?;
            self.skipped.push(wc.clone());
            self.deletion_conflicts.insert(wc.clone(), (base, reason));
            self.emit(wc, NodeKind::None, UpdateAction::TreeConflicted);
            return Ok(());
        }
        let mut items = Vec::new();
        removal_items(&db, wc, &mut items)?;
        db.with_txn(|t| {
            t.base_remove(wc)?;
            if let Some(m) = &marker {
                t.base_add(wc, m)?;
            }
            t.queue_work(&items)
        })?;
        failpoint!("update.after_delete_txn");
        self.run_queue()?;
        self.emit(wc, kind, UpdateAction::Delete);
        Ok(())
    }

    pub(super) fn finish_dir(&mut self, d: DirState) -> Result<()> {
        let db = self.s.db.clone();
        if d.skipped {
            if let Some(tc) = d.tree_conflict {
                self.record_tree_conflict(
                    &d.wc,
                    d.base.as_ref(),
                    &d.new_repos,
                    NodeKind::Dir,
                    tc.reason,
                    tc.action,
                    false,
                )?;
            }
            return Ok(());
        }
        if !d.in_target {
            return Ok(());
        }
        let rev = self.rev()?;
        let old_pristine = d
            .base
            .as_ref()
            .and_then(|b| b.properties.clone())
            .unwrap_or_default();
        // A shadowed directory (deleted or replaced locally, or below an obstruction) only moves
        // BASE on; its working properties are not merged into.
        let mut shadowed = d.shadow.is_some();
        let actual = if shadowed {
            old_pristine.clone()
        } else if d.added || d.base.is_none() {
            Vec::new()
        } else {
            db.actual_props(&d.wc)?
        };
        let mut pm = merge_props(&old_pristine, &d.prop_changes, &actual);
        if d.obstructed && !pm.conflicted.is_empty() {
            // A property conflict needs a marker file inside the directory, which is not there
            // (and a symlink is never written through): only BASE moves on, the local
            // properties stay as they are, and the tree conflict tells the user.
            shadowed = true;
            pm = merge_props(&old_pristine, &d.prop_changes, &old_pristine);
        }
        let mut work: Vec<Skel> = Vec::new();
        let mut conflicts = Vec::new();
        let mut action = None;
        if !pm.conflicted.is_empty() {
            let marker = create_prop_marker(db.wcroot(), d.wc.as_str(), true)?;
            work.push(marker.work_item.to_skel());
            conflicts.push(NewConflict::Prop(PropConflict {
                markers: vec![marker.relpath],
                names: pm.conflicted.clone(),
                old_props: Some(old_pristine.clone()),
                mine_props: Some(actual.clone()),
                their_props: Some(pm.new_pristine.clone()),
            }));
            action = Some(UpdateAction::Conflicted);
        } else if pm.applied || old_pristine != pm.new_pristine {
            action = Some(UpdateAction::Update);
        }
        let old = d.base.as_ref();
        let node = BaseNode {
            kind: NodeKind::Dir,
            presence: Presence::Normal,
            repos_root_url: self.s.repos_root.clone(),
            repos_uuid: self.s.uuid.clone(),
            repos_relpath: d.new_repos.clone(),
            revision: rev,
            changed_rev: d.entry.rev.or_else(|| old.and_then(|b| b.changed_rev)),
            changed_date: d.entry.date.or_else(|| old.and_then(|b| b.changed_date)),
            changed_author: d
                .entry
                .author
                .clone()
                .or_else(|| old.and_then(|b| b.changed_author.clone())),
            depth: Some(d.depth),
            properties: Some(pm.new_pristine.clone()),
            checksum: None,
            symlink_target: None,
            translated_size: None,
            last_mod_time: None,
            inherited_props: None,
            dav_cache: None,
        };
        let overlay = if pm.new_actual == pm.new_pristine {
            None
        } else {
            Some(pm.new_actual.clone())
        };
        let (old_side, new_side) = self.side(old, &d.new_repos, NodeKind::Dir);
        let info = conflict_record::operation_info(
            self.s.switch_relpath.is_some(),
            &self.s.repos_root,
            &self.s.uuid,
            old_side.as_ref(),
            new_side.as_ref(),
        );
        if let Some(tc) = &d.tree_conflict {
            conflicts.push(NewConflict::Tree(tc.reason.clone(), tc.action.clone()));
            action = Some(UpdateAction::TreeConflicted);
        }
        db.with_txn(|t| {
            t.base_add(&d.wc, &node)?;
            if !shadowed && (d.base.is_some() || overlay.is_some()) {
                t.set_actual_props(&d.wc, overlay.as_ref())?;
            }
            if !conflicts.is_empty() {
                conflict_record::record(t, &d.wc, &info, conflicts)?;
            }
            t.queue_work(&work)
        })?;
        self.run_queue()?;
        if let Some(a) = action
            && (!shadowed || d.tree_conflict.is_some())
        {
            self.emit(&d.wc, NodeKind::Dir, a);
        }
        Ok(())
    }

    pub(super) fn finish_edit(&mut self) -> Result<()> {
        if self.is_finished() {
            return Ok(());
        }
        self.run_queue()?;
        let rev = self.rev()?;
        let db = self.s.db.clone();
        let target = self.target_wc.clone();
        let relocation = match (&self.s.switch_relpath, self.old_target()) {
            (Some(new), Some(old)) if !old.is_empty() => Some((old.to_owned(), new.clone())),
            _ => None,
        };
        let skipped = self.skipped.clone();
        let depth = self.s.depth;
        let sticky = self.s.sticky_depth;
        db.with_txn(|t| {
            if let Some((old, new)) = &relocation {
                t.relocate_subtree(&target, old, new)?;
            }
            t.bump_revisions_post_update(&target, rev, depth, &skipped)?;
            if sticky {
                // Directories the server had nothing to send for were never opened; they get
                // their new depth here (svn's server opens them on a depth upgrade).
                t.record_sticky_depth(&target, depth, &skipped)?;
            }
            Ok(())
        })
        .map_err(|e: SvnError| e)?;
        self.mark_finished();
        Ok(())
    }
}
