//! [`WcTxn`]: statement-level writers that run inside ONE caller-owned write transaction.
//!
//! The per-operation `WcDb` methods each open their own transaction. Update and switch must
//! change BASE, the local property overlay, the conflict record and the work queue of a node
//! together, otherwise a crash could leave a new BASE next to a working file derived from the old
//! one (svn does the same in `svn_wc__db_base_add_file` with its `conflict` and `work_items`
//! arguments). [`WcDb::with_txn`] hands out a [`WcTxn`] for exactly that: every call below joins
//! the same `BEGIN IMMEDIATE` transaction, which commits only when the closure returns `Ok`.
//!
//! Keep the closure short and free of network I/O, like any [`WcDb::with_write`] closure.

use rusqlite::{Connection, params};
use svn_types::{Depth, ErrorCode, NodeKind, RelPath, Result, SvnError};

use super::actual;
use super::nodes::{self, Node, Presence, Props, kind_word, parent_text, relpath_depth};
use super::ops::{self, BaseNode, scope_sql};
use super::{WcDb, sql_err};
use crate::conflicts::{Conflict, ConflictSkel, OperationInfo};
use crate::skel::Skel;

/// Typed writers bound to one open write transaction; see the module documentation.
pub struct WcTxn<'a> {
    conn: &'a Connection,
    wc_id: i64,
}

fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| SvnError::new(ErrorCode::Internal, "revision out of range"))
}

impl WcDb {
    /// Runs `f` inside one `BEGIN IMMEDIATE` transaction with typed writers; commits on `Ok`,
    /// rolls everything back on `Err`. Fails like [`WcDb::with_write`] on read-only formats.
    pub fn with_txn<T>(&self, f: impl FnOnce(&WcTxn<'_>) -> Result<T>) -> Result<T> {
        self.with_write(|tx| {
            f(&WcTxn {
                conn: tx,
                wc_id: self.wc_id,
            })
        })
    }
}

impl WcTxn<'_> {
    // ---- reads inside the transaction ----------------------------------------------------

    /// BASE (`op_depth = 0`) row of `path`.
    pub fn read_base(&self, path: &RelPath) -> Result<Option<Node>> {
        nodes::read_base(self.conn, self.wc_id, path)
    }

    /// The `conflict_data` skel recorded for `path`.
    pub fn conflict_skel(&self, path: &RelPath) -> Result<Option<Skel>> {
        Ok(actual::read_conflict(self.conn, self.wc_id, path)?.map(|r| r.skel))
    }

    /// `(root URL, UUID)` of the `REPOSITORY` row `repos_id`.
    pub fn repository(&self, repos_id: i64) -> Result<(String, String)> {
        self.conn
            .query_row(
                "SELECT root, uuid FROM repository WHERE id = ?1",
                [repos_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql_err)
    }

    // ---- BASE layer ---------------------------------------------------------------------

    /// Inserts or replaces the BASE row of `path` (same row as [`WcDb::base_add`]).
    pub fn base_add(&self, path: &RelPath, node: &BaseNode) -> Result<()> {
        ops::base_add(self.conn, self.wc_id, path, node)
    }

    /// Removes the BASE subtree at `path` (same rows as [`WcDb::base_remove`]).
    pub fn base_remove(&self, path: &RelPath) -> Result<()> {
        ops::base_remove(self.conn, self.wc_id, path)
    }

    /// Turns the BASE row of a directory into an `incomplete` one at `rev`, so an interrupted
    /// update is restartable; `repos_relpath` relocates it (switch).
    pub fn mark_incomplete(
        &self,
        path: &RelPath,
        rev: u64,
        repos_relpath: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE nodes SET presence = 'incomplete', revision = ?3, \
                 repos_path = COALESCE(?4, repos_path) \
                 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
                params![self.wc_id, path.as_str(), to_i64(rev)?, repos_relpath],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Rewrites the repository path prefix `old_prefix` to `new_prefix` in the BASE subtree at
    /// `root` (switch).
    pub fn relocate_subtree(
        &self,
        root: &RelPath,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<()> {
        if old_prefix == new_prefix {
            return Ok(());
        }
        let scope = scope_sql();
        self.conn
            .execute(
                &format!(
                    "UPDATE nodes SET repos_path = CASE WHEN repos_path = ?3 THEN ?4 \
                     ELSE ?4 || substr(repos_path, length(?3) + 1) END \
                     WHERE wc_id = ?1 AND op_depth = 0 AND {scope} AND repos_path IS NOT NULL \
                     AND (repos_path = ?3 OR substr(repos_path, 1, length(?3) + 1) = ?3 || '/')"
                ),
                params![self.wc_id, root.as_str(), old_prefix, new_prefix],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// [`WcTxn::bump_revisions_post_update`] at depth infinity.
    pub fn bump_revisions(&self, root: &RelPath, rev: u64, skipped: &[RelPath]) -> Result<()> {
        self.bump_revisions_post_update(root, rev, Depth::Infinity, skipped)
    }

    /// Moves the BASE tree at `root` to `rev` after an update drive, like svn's
    /// `bump_revisions_post_update`: nodes within `depth` of `root` get the new revision, except
    /// the `skipped` subtrees (tree conflicts), which keep theirs. Below the root, a
    /// `not-present` node that the drive did not re-add is gone at `rev` and its row is removed,
    /// as is a `server-excluded` node at another revision. The root itself keeps such a row (an
    /// update target deleted on the server stays a `not-present` marker).
    pub fn bump_revisions_post_update(
        &self,
        root: &RelPath,
        rev: u64,
        depth: Depth,
        skipped: &[RelPath],
    ) -> Result<()> {
        let depth = match depth {
            Depth::Unknown | Depth::Exclude => Depth::Infinity,
            d => d,
        };
        self.bump_node(root, to_i64(rev)?, depth, skipped, true, false)
    }

    fn bump_node(
        &self,
        path: &RelPath,
        rev: i64,
        depth: Depth,
        skipped: &[RelPath],
        is_root: bool,
        skip_when_dir: bool,
    ) -> Result<()> {
        if skipped.contains(path) {
            return Ok(());
        }
        let Some(node) = self.read_base(path)? else {
            return Ok(());
        };
        if skip_when_dir && node.kind == NodeKind::Dir {
            return Ok(());
        }
        let old_rev = node.revision.and_then(|r| i64::try_from(r).ok());
        if !is_root
            && (node.presence == Presence::NotPresent
                || (node.presence == Presence::ServerExcluded && old_rev != Some(rev)))
        {
            return ops::base_remove(self.conn, self.wc_id, path);
        }
        if old_rev.is_some() && old_rev != Some(rev) {
            self.conn
                .execute(
                    "UPDATE nodes SET revision = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 \
                     AND op_depth = 0",
                    params![self.wc_id, path.as_str(), rev],
                )
                .map_err(sql_err)?;
        }
        if depth == Depth::Empty
            || node.kind != NodeKind::Dir
            || matches!(
                node.presence,
                Presence::ServerExcluded | Presence::Excluded | Presence::NotPresent
            )
        {
            return Ok(());
        }
        let below = if matches!(depth, Depth::Immediates | Depth::Files) {
            Depth::Empty
        } else {
            depth
        };
        for child in self.base_children(path)? {
            self.bump_node(&child, rev, below, skipped, false, depth == Depth::Files)?;
        }
        Ok(())
    }

    /// Paths of the BASE children of `path`.
    fn base_children(&self, path: &RelPath) -> Result<Vec<RelPath>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT local_relpath FROM nodes WHERE wc_id = ?1 AND parent_relpath = ?2 \
                 AND op_depth = 0 ORDER BY local_relpath",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![self.wc_id, path.as_str()], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(RelPath::new(&row.map_err(sql_err)?)?);
        }
        Ok(out)
    }

    // ---- working layers -----------------------------------------------------------------

    /// Turns the BASE subtree at `path` into a local copy of itself (a working layer at the
    /// depth of `path`), as svn does when an incoming delete meets local modifications: the
    /// content stays versioned as an addition with history and the tree conflict decides what
    /// happens next.
    ///
    /// Fails without changing anything when a working row already exists at that depth in the
    /// subtree (a local replacement must never be overwritten).
    pub fn convert_base_to_copy(&self, path: &RelPath) -> Result<()> {
        let scope = scope_sql();
        let taken: i64 = self
            .conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM nodes WHERE wc_id = ?1 AND op_depth = ?3 AND {scope}"
                ),
                params![self.wc_id, path.as_str(), relpath_depth(path)],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if taken > 0 {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("cannot keep '{path}' as a local copy: a working layer already exists"),
            ));
        }
        self.conn
            .execute(
                &format!(
                    "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                     repos_id, repos_path, revision, presence, kind, checksum, symlink_target, \
                     properties, translated_size, last_mod_time, changed_revision, changed_date, \
                     changed_author, depth) \
                     SELECT wc_id, local_relpath, ?3, parent_relpath, repos_id, repos_path, \
                     revision, presence, kind, checksum, symlink_target, properties, \
                     translated_size, last_mod_time, changed_revision, changed_date, \
                     changed_author, depth FROM nodes \
                     WHERE wc_id = ?1 AND op_depth = 0 AND presence = 'normal' AND {scope}"
                ),
                params![self.wc_id, path.as_str(), relpath_depth(path)],
            )
            .map_err(sql_err)?;
        self.conn
            .execute(
                &format!("DELETE FROM nodes WHERE wc_id = ?1 AND op_depth = 0 AND {scope}"),
                params![self.wc_id, path.as_str()],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Adds a `base-deleted` working row over the BASE node at `path`: the node exists in the
    /// repository, but what is on disk is not part of it (an obstruction).
    pub fn mark_base_deleted(&self, path: &RelPath, kind: NodeKind) -> Result<()> {
        self.mark_base_deleted_at(path, kind, relpath_depth(path))
    }

    /// Like [`WcTxn::mark_base_deleted`], with the working row at `op_depth`: the nodes below
    /// an obstructed directory are shadowed by a layer rooted at that directory.
    pub fn mark_base_deleted_at(
        &self,
        path: &RelPath,
        kind: NodeKind,
        op_depth: u32,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                 presence, kind) VALUES (?1, ?2, ?3, ?4, 'base-deleted', ?5)",
                params![
                    self.wc_id,
                    path.as_str(),
                    op_depth,
                    parent_text(path),
                    kind_word(kind)
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    // ---- ACTUAL_NODE --------------------------------------------------------------------

    /// Stores the working property overlay of `path`; `None` clears it (the caller decides
    /// whether the working set differs from the pristine one).
    pub fn set_actual_props(&self, path: &RelPath, props: Option<&Props>) -> Result<()> {
        actual::write_props(self.conn, self.wc_id, path, props)
    }

    /// Adds `conflicts` to the conflict record of `path`: a conflict of a kind that is already
    /// recorded replaces the old one, and the operation info is replaced by `info`. Legacy
    /// conflict columns and the `older/left/right_checksum` columns are left untouched.
    pub fn add_conflicts(
        &self,
        path: &RelPath,
        info: &OperationInfo,
        conflicts: impl IntoIterator<Item = Conflict>,
    ) -> Result<()> {
        let mut skel = match self.conflict_skel(path)? {
            Some(s) => ConflictSkel::from_skel(&s)?,
            None => ConflictSkel::default(),
        };
        skel.operation = Some(info.clone());
        for c in conflicts {
            skel.conflicts
                .retain(|e| std::mem::discriminant(e) != std::mem::discriminant(&c));
            skel.add(c)?;
        }
        actual::write_conflict_data(self.conn, self.wc_id, path, &skel.to_skel())
    }

    // ---- stat cache and work queue ------------------------------------------------------

    /// Stores the stat cache of the current file node (`PathNotFound` when unversioned).
    pub fn record_fileinfo(
        &self,
        path: &RelPath,
        translated_size: Option<i64>,
        last_mod_time: Option<i64>,
    ) -> Result<()> {
        ops::record_fileinfo(self.conn, self.wc_id, path, translated_size, last_mod_time)
    }

    /// Appends serialized work items to the queue.
    pub fn queue_work(&self, items: &[Skel]) -> Result<()> {
        ops::queue_work(self.conn, items)
    }
}
