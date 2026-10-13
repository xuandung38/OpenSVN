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
use crate::wc::{inspect, txn};

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
        if !base.presence.is_present() {
            db.base_remove(wc, &[])?;
            return Ok(());
        }
        let kind = base.kind;
        if db.conflict(wc)?.is_some() {
            self.skipped.push(wc.clone());
            self.emit(wc, kind, UpdateAction::Skipped);
            return Ok(());
        }
        if inspect::subtree_has_local_changes(&db, wc)? {
            let replaced = db.read_node(wc)?.is_some_and(|c| {
                c.op_depth > 0 && c.presence != Presence::BaseDeleted
            });
            let reason = if replaced {
                TreeReason::Replaced
            } else {
                TreeReason::Edited
            };
            let repos = base.repos_relpath.clone().unwrap_or_default();
            return self.record_tree_conflict(wc, Some(&base), &repos, NodeKind::None, reason, TreeAction::Delete);
        }
        let mut items = Vec::new();
        removal_items(&db, wc, &mut items)?;
        db.base_remove(wc, &items)?;
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
                )?;
            }
            return Ok(());
        }
        if !d.in_target {
            return Ok(());
        }
        let rev = self.rev()?;
        let wc_id = db.wc_id();
        let old_pristine = d
            .base
            .as_ref()
            .and_then(|b| b.properties.clone())
            .unwrap_or_default();
        let actual = if d.added || d.base.is_none() {
            Vec::new()
        } else {
            db.actual_props(&d.wc)?
        };
        let pm = merge_props(&old_pristine, &d.prop_changes, &actual);
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
        }
        db.with_write(|tx| {
            txn::base_add(tx, wc_id, &d.wc, &node)?;
            if d.base.is_some() || overlay.is_some() {
                txn::set_actual_props(tx, wc_id, &d.wc, overlay.as_ref())?;
            }
            if !conflicts.is_empty() {
                conflict_record::record(tx, wc_id, &d.wc, &info, conflicts)?;
            }
            txn::queue_work(tx, &work)
        })?;
        self.run_queue()?;
        if let Some(a) = action {
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
        let wc_id = db.wc_id();
        let target = self.target_wc.clone();
        let relocation = match (&self.s.switch_relpath, self.old_target()) {
            (Some(new), Some(old)) if !old.is_empty() => Some((old.to_owned(), new.clone())),
            _ => None,
        };
        let skipped = self.skipped.clone();
        db.with_write(|tx| {
            if let Some((old, new)) = &relocation {
                txn::relocate_subtree(tx, wc_id, &target, old, new)?;
            }
            txn::bump_revisions(tx, wc_id, &target, rev, &skipped)
        })
        .map_err(|e: SvnError| e)?;
        self.mark_finished();
        Ok(())
    }
}
