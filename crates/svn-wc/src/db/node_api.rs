//! `WcDb` methods for the NODES model, property/changelist/conflict state and work items.
//!
//! Write methods take `work`: serialized work items queued in the same transaction as the row
//! changes, so a crash can never leave the rows changed without the file operation recorded.

use rusqlite::OptionalExtension;
use svn_types::{Depth, NodeKind, RelPath, Result};

use super::actual::{self, ConflictRecord};
use super::nodes::{self, Node, Props};
use super::ops::{self, BaseNode, CommitInfo};
use super::sql_err;
use super::{WcDb, failpoint};
use crate::skel::Skel;

/// Stat cache of a working file, stored with its node by [`WcDb::wq_record_and_fetch_next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileInfo {
    /// Size of the file after translation, when known.
    pub translated_size: Option<i64>,
    /// Modification time (APR microseconds) when the file was last known unmodified.
    pub last_mod_time: Option<i64>,
}

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

    /// The node whose `moved_to` names `path` (the source of a local move to `path`), if any.
    pub fn moved_from(&self, path: &RelPath) -> Result<Option<RelPath>> {
        let found: Option<String> = self.with_read(|c| {
            c.query_row(
                "SELECT local_relpath FROM nodes WHERE wc_id = ?1 AND moved_to = ?2 \
                 ORDER BY op_depth LIMIT 1",
                rusqlite::params![self.wc_id, path.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)
        })?;
        found.map(|p| RelPath::new(&p)).transpose()
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

    /// Finishes a work item and fetches the next one in ONE transaction, like
    /// `svn_wc__db_wq_record_and_fetch_next`: deletes item `completed_id` (`None` for the first
    /// fetch, when nothing is finished yet), stores the stat cache of every `(path, info)` entry,
    /// and returns the lowest-id remaining item (`None` when the queue is empty). Either all of it
    /// is durable or none, so a crash can neither replay a finished item without its fileinfo nor
    /// lose the fileinfo of a finished item.
    ///
    /// A `fileinfo` path whose node no longer exists is skipped silently (an earlier item of the
    /// same queue may have deleted it); the item is still completed.
    ///
    /// Failpoints: `wc.wq.after_delete` and `wc.wq.after_fileinfo` fire between the steps.
    pub fn wq_record_and_fetch_next(
        &self,
        completed_id: Option<i64>,
        fileinfo: &[(RelPath, FileInfo)],
    ) -> Result<Option<(i64, Skel)>> {
        self.wq_record_and_fetch_next_raw(completed_id, fileinfo)?
            .map(|(id, blob)| Ok((id, Skel::parse(&blob)?)))
            .transpose()
    }

    /// [`wq_record_and_fetch_next`](Self::wq_record_and_fetch_next) returning the stored bytes
    /// of the next item unparsed (the work queue runner decodes them itself).
    pub(crate) fn wq_record_and_fetch_next_raw(
        &self,
        completed_id: Option<i64>,
        fileinfo: &[(RelPath, FileInfo)],
    ) -> Result<Option<(i64, Vec<u8>)>> {
        self.with_write(|tx| {
            if let Some(id) = completed_id {
                tx.execute("DELETE FROM work_queue WHERE id = ?1", [id])
                    .map_err(sql_err)?;
            }
            failpoint!("wc.wq.after_delete");
            for (path, info) in fileinfo {
                match ops::record_fileinfo(
                    tx,
                    self.wc_id,
                    path,
                    info.translated_size,
                    info.last_mod_time,
                ) {
                    Err(e) if e.code == svn_types::ErrorCode::PathNotFound => {}
                    other => other?,
                }
            }
            failpoint!("wc.wq.after_fileinfo");
            tx.query_row(
                "SELECT id, work FROM work_queue ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)
        })
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

    /// Folds one committed path into BASE. Prefer [`WcDb::commit_nodes`] for a whole commit.
    pub fn commit_node(&self, path: &RelPath, info: &CommitInfo, work: &[Skel]) -> Result<()> {
        self.commit_nodes(&[(path.clone(), info.clone())], work)
    }

    /// Post-commit processing of every committed path in ONE transaction (`bump_revisions`): a
    /// crash can never leave the working copy half-committed. Order the batch parents before
    /// children; for a committed delete pass only the root of the deletion. Committing a path
    /// that an earlier entry of the same batch (or an earlier call) already folded into BASE as
    /// `not-present` at `new_rev` is a no-op, so the batch can list every item svn gathers, e.g.
    /// `[e, e/f2]` for a replaced directory whose child was deleted inside the copy.
    pub fn commit_nodes(&self, batch: &[(RelPath, CommitInfo)], work: &[Skel]) -> Result<()> {
        self.write_with_work(work, |c| {
            for (path, info) in batch {
                ops::commit_node(c, self.wc_id, path, info)?;
            }
            Ok(())
        })
    }
}
