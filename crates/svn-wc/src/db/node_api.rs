//! `WcDb` methods for the NODES model, property/changelist/conflict state and work items.
//!
//! Write methods take `work`: serialized work items queued in the same transaction as the row
//! changes, so a crash can never leave the rows changed without the file operation recorded.

use svn_types::{Depth, NodeKind, RelPath, Result};

use super::WcDb;
use super::actual::{self, ConflictRecord};
use super::nodes::{self, Node, Props};
use super::ops::{self, BaseNode, CommitInfo};
use super::sql_err;
use crate::skel::Skel;

impl WcDb {
    // ---- reads ---------------------------------------------------------------------------

    /// Current state of `path` (the highest `op_depth` row, i.e. `NODES_CURRENT`).
    pub fn read_node(&self, path: &RelPath) -> Result<Option<Node>> {
        self.with_read(|c| nodes::read_current(c, self.wc_id, path))
    }

    /// BASE (`op_depth = 0`) state of `path`.
    pub fn read_base(&self, path: &RelPath) -> Result<Option<Node>> {
        self.with_read(|c| nodes::read_base(c, self.wc_id, path))
    }

    /// Paths directly below `parent` (any layer, any presence), sorted.
    pub fn children(&self, parent: &RelPath) -> Result<Vec<RelPath>> {
        self.with_read(|c| nodes::children(c, self.wc_id, parent))
    }

    /// Pristine properties of the current node (empty when it has none).
    pub fn pristine_props(&self, path: &RelPath) -> Result<Props> {
        self.with_read(|c| ops::pristine_props(c, self.wc_id, path))
    }

    /// Working properties: local changes when present, else the pristine ones.
    pub fn actual_props(&self, path: &RelPath) -> Result<Props> {
        self.with_read(|c| match actual::read_props(c, self.wc_id, path)? {
            Some(p) => Ok(p),
            None => ops::pristine_props(c, self.wc_id, path),
        })
    }

    /// Changelist membership.
    pub fn changelist(&self, path: &RelPath) -> Result<Option<String>> {
        self.with_read(|c| actual::read_changelist(c, self.wc_id, path))
    }

    /// Conflict record of `path`, if any.
    pub fn conflict(&self, path: &RelPath) -> Result<Option<ConflictRecord>> {
        self.with_read(|c| actual::read_conflict(c, self.wc_id, path))
    }

    /// Pending work queue items in queue order as `(id, skel)`.
    pub fn work_items(&self) -> Result<Vec<(i64, Skel)>> {
        self.with_read(|c| {
            let mut stmt = c
                .prepare("SELECT id, work FROM work_queue ORDER BY id")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
                .map_err(sql_err)?;
            let mut out = Vec::new();
            for row in rows {
                let (id, blob) = row.map_err(sql_err)?;
                out.push((id, Skel::parse(&blob)?));
            }
            Ok(out)
        })
    }

    // ---- writes (one transaction each, plus the caller's work items) -----------------------

    fn write_with_work<T>(
        &self,
        work: &[Skel],
        f: impl FnOnce(&rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        self.with_write(|tx| {
            let value = f(tx)?;
            ops::queue_work(tx, work)?;
            Ok(value)
        })
    }

    /// Queues work items without any other change.
    pub fn queue_work(&self, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |_| Ok(()))
    }

    /// Removes a finished work item.
    pub fn complete_work_item(&self, id: i64) -> Result<()> {
        self.with_write(|tx| {
            tx.execute("DELETE FROM work_queue WHERE id = ?1", [id])
                .map_err(sql_err)?;
            Ok(())
        })
    }

    /// Inserts or replaces the BASE node of `path` (checkout/update).
    pub fn base_add(&self, path: &RelPath, node: &BaseNode, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::base_add(c, self.wc_id, path, node))
    }

    /// Removes the BASE node and its BASE descendants.
    pub fn base_remove(&self, path: &RelPath, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::base_remove(c, self.wc_id, path))
    }

    /// Sets the directory depth of a BASE directory.
    pub fn set_depth(&self, path: &RelPath, depth: Depth) -> Result<()> {
        self.write_with_work(&[], |c| ops::set_depth(c, self.wc_id, path, depth))
    }

    /// Stores the stat cache (`translated_size`, `last_mod_time`) of a file.
    pub fn record_fileinfo(
        &self,
        path: &RelPath,
        translated_size: Option<i64>,
        last_mod_time: Option<i64>,
    ) -> Result<()> {
        self.write_with_work(&[], |c| {
            ops::record_fileinfo(c, self.wc_id, path, translated_size, last_mod_time)
        })
    }

    /// `svn add`: schedules a new file, directory or symlink.
    pub fn op_add(
        &self,
        path: &RelPath,
        kind: NodeKind,
        symlink_target: Option<&str>,
        work: &[Skel],
    ) -> Result<()> {
        self.write_with_work(work, |c| {
            ops::op_add(c, self.wc_id, path, kind, symlink_target)
        })
    }

    /// `svn delete`: schedules the node and its subtree for deletion.
    pub fn op_delete(&self, path: &RelPath, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::op_delete(c, self.wc_id, path))
    }

    /// `svn copy` within the working copy.
    pub fn op_copy(&self, src: &RelPath, dst: &RelPath, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::op_copy(c, self.wc_id, src, dst, false))
    }

    /// `svn move` within the working copy.
    pub fn op_move(&self, src: &RelPath, dst: &RelPath, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::op_move(c, self.wc_id, src, dst))
    }

    /// `svn revert`: drops local changes at `path` (whole layer for an operation root).
    pub fn op_revert(&self, path: &RelPath, recursive: bool, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::op_revert(c, self.wc_id, path, recursive))
    }

    /// Sets working properties (a set equal to the pristine one clears the local change).
    pub fn set_props(&self, path: &RelPath, props: &Props, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::set_props(c, self.wc_id, path, props))
    }

    /// Adds `path` to a changelist, or removes it with `None`.
    pub fn set_changelist(&self, path: &RelPath, name: Option<&str>) -> Result<()> {
        self.write_with_work(&[], |c| ops::set_changelist(c, self.wc_id, path, name))
    }

    /// Records a conflict on `path`.
    pub fn mark_conflict(
        &self,
        path: &RelPath,
        record: &ConflictRecord,
        work: &[Skel],
    ) -> Result<()> {
        self.write_with_work(work, |c| {
            actual::write_conflict(c, self.wc_id, path, record)
        })
    }

    /// Clears the conflict record of `path`.
    pub fn resolve(&self, path: &RelPath, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| actual::clear_conflict(c, self.wc_id, path))
    }

    /// Folds one committed path into BASE (`bump_revisions` after a commit). Process parents
    /// before children; for a committed delete pass only the root of the deletion.
    pub fn commit_node(&self, path: &RelPath, info: &CommitInfo, work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| ops::commit_node(c, self.wc_id, path, info))
    }
}
