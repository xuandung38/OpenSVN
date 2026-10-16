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
use super::misc::LockInfo;
use super::nodes::{self, Node, Presence, Props, kind_word, parent_text, relpath_depth};
use super::ops::{self, BaseNode, CommitInfo, scope_sql};
use super::{WcDb, sql_err};
use crate::conflicts::{Conflict, ConflictSkel, OperationInfo};
use crate::pristine::PristineRecord;
use crate::skel::Skel;
use crate::workqueue::Unsettled;

/// Typed writers bound to one open write transaction; see the module documentation.
pub struct WcTxn<'a> {
    conn: &'a Connection,
    wc_id: i64,
    db: &'a WcDb,
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
                db: self,
            })
        })
    }
}

impl WcTxn<'_> {
    pub(super) fn conn(&self) -> &Connection {
        self.conn
    }

    pub(super) fn wc_id(&self) -> i64 {
        self.wc_id
    }

    /// Fails with [`ErrorCode::WcBusy`] when another client (`svn cleanup`) broke a working copy
    /// lock this handle holds: its WC_LOCK row is gone (or has other levels) or its marker in
    /// `.svn/tmp` is. The lock is then forgotten and its release leaves the row alone. A long
    /// operation that relies on its lock calls this first in every transaction, so nothing is
    /// written once the lock is gone.
    pub fn verify_wc_lock(&self) -> Result<()> {
        self.db.wc_lock_verify_in(self.conn)
    }

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
        super::connection::cached_row(
            self.conn,
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
        super::connection::cached_exec(
            self.conn,
            "UPDATE nodes SET presence = 'incomplete', revision = ?3, \
                 repos_path = COALESCE(?4, repos_path) \
                 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
            params![self.wc_id, path.as_str(), to_i64(rev)?, repos_relpath],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// Moves the BASE subtree at `root` from `old_prefix` to `new_prefix` after a switch: a node
    /// whose repository path is still `old_prefix` plus its path below `root` (it was not
    /// switched on its own and the drive did not rewrite it) gets `new_prefix` plus that path.
    /// Nodes the drive already moved, and nodes switched elsewhere, keep their path, so the
    /// rewrite is idempotent even when `new_prefix` lies below `old_prefix`.
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
        // `suffix`: the path below `root` ('' for the root itself); `under(p)`: `p/suffix`.
        let suffix = "(CASE WHEN local_relpath = ?2 THEN '' \
            WHEN ?2 = '' THEN local_relpath \
            ELSE substr(local_relpath, length(?2) + 2) END)";
        let under = |p: &str| {
            format!(
                "(CASE WHEN {suffix} = '' THEN {p} WHEN {p} = '' THEN {suffix} \
                 ELSE {p} || '/' || {suffix} END)"
            )
        };
        super::connection::cached_exec(
            self.conn,
            &format!(
                "UPDATE nodes SET repos_path = {} \
                     WHERE wc_id = ?1 AND op_depth = 0 AND {scope} AND repos_path IS NOT NULL \
                     AND repos_path = {}",
                under("?4"),
                under("?3")
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
        let Some(node) = self.read_base(root)? else {
            return Ok(());
        };
        self.bump_node(node, to_i64(rev)?, depth, skipped, None, true, false)
    }

    /// [`WcTxn::bump_revisions_post_update`] after a switch of `root` to `new_repos_relpath`:
    /// every node it bumps also gets the repository path below `new_repos_relpath` that its
    /// place in the tree gives it, like svn's `bump_node_revision` with a new repository path.
    /// Nodes the drive did not open (nothing changed between the old and the new location) are
    /// moved here; so are the ones a resumed drive left at their old location because they had
    /// no change to receive. The `skipped` subtrees and file externals keep their location.
    pub fn bump_revisions_post_switch(
        &self,
        root: &RelPath,
        rev: u64,
        depth: Depth,
        skipped: &[RelPath],
        new_repos_relpath: &str,
    ) -> Result<()> {
        let depth = match depth {
            Depth::Unknown | Depth::Exclude => Depth::Infinity,
            d => d,
        };
        let Some(node) = self.read_base(root)? else {
            return Ok(());
        };
        self.bump_node(
            node,
            to_i64(rev)?,
            depth,
            skipped,
            Some(new_repos_relpath),
            true,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn bump_node(
        &self,
        node: Node,
        rev: i64,
        depth: Depth,
        skipped: &[RelPath],
        repos_relpath: Option<&str>,
        is_root: bool,
        skip_when_dir: bool,
    ) -> Result<()> {
        let path = &node.relpath;
        if skipped.contains(path) {
            return Ok(());
        }
        if skip_when_dir && node.kind == NodeKind::Dir {
            return Ok(());
        }
        // A file external is an update root of its own: the parent's update leaves it alone.
        if !is_root && node.file_external {
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
            super::connection::cached_exec(
                self.conn,
                "UPDATE nodes SET revision = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 \
                     AND op_depth = 0",
                params![self.wc_id, path.as_str(), rev],
            )
            .map_err(sql_err)?;
        }
        if let Some(repos) = repos_relpath
            && node.repos_relpath.as_deref() != Some(repos)
        {
            super::connection::cached_exec(
                self.conn,
                "UPDATE nodes SET repos_path = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 \
                     AND op_depth = 0",
                params![self.wc_id, path.as_str(), repos],
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
        // One query per directory; each child's row comes with it.
        for child in nodes::base_children(self.conn, self.wc_id, path)? {
            let child_repos = repos_relpath.map(|r| {
                let name = child.relpath.file_name().unwrap_or("");
                if r.is_empty() {
                    name.to_owned()
                } else {
                    format!("{r}/{name}")
                }
            });
            self.bump_node(
                child,
                rev,
                below,
                skipped,
                child_repos.as_deref(),
                false,
                depth == Depth::Files,
            )?;
        }
        Ok(())
    }

    /// Folds a committed path into BASE (same rows as [`WcDb::commit_nodes`] for one entry):
    /// parents before children, idempotent when the path was already committed at
    /// `info.new_rev`.
    pub fn commit_node(&self, path: &RelPath, info: &CommitInfo) -> Result<()> {
        ops::commit_node(self.conn, self.wc_id, path, info)
    }

    // ---- repository lock cache ----------------------------------------------------------

    /// Caches `lock` for the BASE node at `path` (`svn_wc__db_lock_add`): the row is keyed by
    /// the node's repository location and replaces an older one. `PathNotFound` without BASE.
    pub fn lock_add(&self, path: &RelPath, lock: &LockInfo) -> Result<()> {
        let n = super::connection::cached_exec(
            self.conn,
            "INSERT OR REPLACE INTO lock (repos_id, repos_relpath, lock_token, lock_owner, \
                 lock_comment, lock_date) SELECT repos_id, repos_path, ?3, ?4, ?5, ?6 FROM nodes \
                 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0 \
                 AND repos_id IS NOT NULL AND repos_path IS NOT NULL",
            params![
                self.wc_id,
                path.as_str(),
                lock.token,
                lock.owner,
                lock.comment,
                lock.date
            ],
        )
        .map_err(sql_err)?;
        if n == 0 {
            return Err(SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{path}' has no BASE node to lock"),
            ));
        }
        Ok(())
    }

    /// Drops the cached lock of the BASE node at `path` (`svn_wc__db_lock_remove`). A path
    /// without BASE or without a cached lock is left alone, so this can be repeated.
    pub fn lock_remove(&self, path: &RelPath) -> Result<()> {
        super::connection::cached_exec(
            self.conn,
            "DELETE FROM lock WHERE EXISTS (SELECT 1 FROM nodes n WHERE n.wc_id = ?1 \
                 AND n.local_relpath = ?2 AND n.op_depth = 0 AND n.repos_id = lock.repos_id \
                 AND n.repos_path = lock.repos_relpath)",
            params![self.wc_id, path.as_str()],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// Like [`lock_remove`](Self::lock_remove), but only when the cached lock still has
    /// `token`: a lock taken again since then is left alone.
    pub fn lock_remove_token(&self, path: &RelPath, token: &str) -> Result<()> {
        super::connection::cached_exec(
            self.conn,
            "DELETE FROM lock WHERE lock_token = ?3 AND EXISTS (SELECT 1 FROM nodes n \
                 WHERE n.wc_id = ?1 AND n.local_relpath = ?2 AND n.op_depth = 0 \
                 AND n.repos_id = lock.repos_id AND n.repos_path = lock.repos_relpath)",
            params![self.wc_id, path.as_str(), token],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// Paths of the BASE children of `path`.
    /// After a `--set-depth` drive: records `depth` on the BASE directory `root` when it
    /// widens it, and with `Infinity` on every present BASE directory below `root` outside the
    /// `skipped` subtrees (svn's `close_directory` rule for sticky depths).
    pub fn record_sticky_depth(
        &self,
        root: &RelPath,
        depth: Depth,
        skipped: &[RelPath],
    ) -> Result<()> {
        fn rank(d: Option<Depth>) -> u8 {
            match d {
                Some(Depth::Empty) => 0,
                Some(Depth::Files) => 1,
                Some(Depth::Immediates) => 2,
                _ => 3,
            }
        }
        if !matches!(
            depth,
            Depth::Empty | Depth::Files | Depth::Immediates | Depth::Infinity
        ) || skipped.iter().any(|s| root.starts_with(s))
        {
            return Ok(());
        }
        let Some(node) = self.read_base(root)? else {
            return Ok(());
        };
        if node.kind != NodeKind::Dir || !node.presence.is_present() {
            return Ok(());
        }
        if rank(Some(depth)) > rank(node.depth) {
            ops::set_depth(self.conn, self.wc_id, root, depth)?;
        }
        if depth != Depth::Infinity {
            return Ok(());
        }
        for child in self.base_children(root)? {
            self.record_sticky_depth(&child, depth, skipped)?;
        }
        Ok(())
    }

    fn base_children(&self, path: &RelPath) -> Result<Vec<RelPath>> {
        self.base_children_where(path, "")
    }

    /// The BASE children a copy of `path` takes: all but file externals (svn's
    /// `STMT_SELECT_OP_DEPTH_CHILDREN`).
    fn base_children_to_copy(&self, path: &RelPath) -> Result<Vec<RelPath>> {
        self.base_children_where(path, " AND file_external IS NULL")
    }

    fn base_children_where(&self, path: &RelPath, filter: &str) -> Result<Vec<RelPath>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT local_relpath FROM nodes WHERE wc_id = ?1 AND parent_relpath = ?2 \
                 AND op_depth = 0{filter} ORDER BY local_relpath"
            ))
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
    /// The layout is svn's (`svn_wc__db_op_make_copy_internal` and `make_copy_txn`): a node at
    /// its parent's revision and repository path joins the parent's layer; one at another
    /// revision (a mixed-revision subtree, or the children of a directory an interrupted update
    /// left `incomplete`) becomes a copy of its own at its own depth, with a `not-present` row in
    /// the parent's layer. Presence is copied as it is (`incomplete` included).
    ///
    /// File externals below `path` are not part of the copy (svn's
    /// `STMT_SELECT_OP_DEPTH_CHILDREN`): their BASE rows go with the rest of BASE.
    ///
    /// Fails without changing anything when a working row already exists at that depth in the
    /// subtree (a local replacement must never be overwritten), or when a node below `path` is
    /// `server-excluded` (svn's `catch_copy_of_server_excluded`: a copy that leaves out what
    /// the user may not read would delete it on commit).
    pub fn convert_base_to_copy(&self, path: &RelPath) -> Result<()> {
        let scope = scope_sql();
        let op_depth = relpath_depth(path);
        let excluded: Option<String> = self
            .conn
            .query_row(
                &format!(
                    "SELECT local_relpath FROM nodes WHERE wc_id = ?1 AND op_depth = 0 \
                     AND presence = 'server-excluded' AND {} LIMIT 1",
                    nodes::strict_descendant("local_relpath", "?2")
                ),
                params![self.wc_id, path.as_str()],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(sql_err(e)),
            })?;
        if let Some(excluded) = excluded {
            return Err(SvnError::new(
                ErrorCode::Forbidden,
                format!("Cannot copy '{excluded}' excluded by server"),
            ));
        }
        let taken: i64 = self
            .conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM nodes WHERE wc_id = ?1 AND op_depth = ?3 AND {scope}"
                ),
                params![self.wc_id, path.as_str(), op_depth],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        if taken > 0 {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("cannot keep '{path}' as a local copy: a working layer already exists"),
            ));
        }
        // A `base-deleted` layer over the BASE subtree first (svn's
        // STMT_INSERT_DELETE_FROM_NODE_RECURSIVE); the copy then replaces it row by row.
        self.conn
            .execute(
                &format!(
                    "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, presence, \
                     kind) SELECT wc_id, local_relpath, ?3, parent_relpath, 'base-deleted', kind \
                     FROM nodes WHERE wc_id = ?1 AND op_depth = 0 AND {scope} \
                     AND (local_relpath = ?2 OR (presence NOT IN ('base-deleted', \
                     'not-present', 'excluded', 'server-excluded') AND file_external IS NULL))"
                ),
                params![self.wc_id, path.as_str(), op_depth],
            )
            .map_err(sql_err)?;
        self.make_copy(path, None, op_depth, false)?;
        // `base-deleted` rows left right above BASE below `path` cover nothing once BASE is gone
        // (svn's STMT_DELETE_WORKING_BASE_DELETE_RECURSIVE in `db_base_remove`).
        self.conn
            .execute(
                &format!(
                    "DELETE FROM nodes WHERE wc_id = ?1 AND {} \
                     AND presence = 'base-deleted' AND op_depth > 0 \
                     AND op_depth = (SELECT MIN(op_depth) FROM nodes n \
                     WHERE n.wc_id = ?1 AND n.local_relpath = nodes.local_relpath \
                     AND op_depth > 0)",
                    nodes::strict_descendant("local_relpath", "?2")
                ),
                params![self.wc_id, path.as_str()],
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

    /// svn's `make_copy_txn` without the move bookkeeping: copies the BASE node `path` into the
    /// layer at `last_op_depth` when it continues its parent (`last`: the parent's repository
    /// id, path and revision), into a layer of its own otherwise.
    fn make_copy(
        &self,
        path: &RelPath,
        last: Option<(i64, &str, i64)>,
        last_op_depth: u32,
        mut shadowed: bool,
    ) -> Result<()> {
        let mut op_depth = relpath_depth(path);
        let mut have_row = false;
        if last_op_depth != op_depth {
            have_row = self.row_exists(path, op_depth)?;
            shadowed |= have_row;
        }
        let (kind, revision, repos_relpath, repos_id): (
            String,
            Option<i64>,
            Option<String>,
            Option<i64>,
        ) = self
            .conn
            .query_row(
                "SELECT kind, revision, repos_path, repos_id FROM nodes \
                     WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
                params![self.wc_id, path.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(sql_err)?;
        if let (Some((last_id, last_repos, last_rev)), Some(repos), Some(id), Some(rev)) =
            (last, repos_relpath.as_deref(), repos_id, revision)
            && id == last_id
            && rev == last_rev
        {
            let name = path.file_name().unwrap_or("");
            let continues = if last_repos.is_empty() {
                repos == name
            } else {
                repos
                    .strip_prefix(last_repos)
                    .and_then(|r| r.strip_prefix('/'))
                    == Some(name)
            };
            if continues {
                op_depth = last_op_depth;
            }
        }
        if !have_row || op_depth == last_op_depth {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                     repos_id, repos_path, revision, presence, kind, checksum, symlink_target, \
                     properties, translated_size, last_mod_time, changed_revision, changed_date, \
                     changed_author, depth) \
                     SELECT wc_id, local_relpath, ?3, parent_relpath, repos_id, repos_path, \
                     revision, presence, kind, checksum, symlink_target, properties, \
                     translated_size, last_mod_time, changed_revision, changed_date, \
                     changed_author, depth FROM nodes \
                     WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
                    params![self.wc_id, path.as_str(), op_depth],
                )
                .map_err(sql_err)?;
            if shadowed && !path.is_root() {
                self.extend_parent_delete(path, &kind, op_depth)?;
            }
            if kind == "dir" {
                let children = self.base_children_to_copy(path)?;
                let me = repos_relpath.as_deref().zip(repos_id).zip(revision);
                for child in children {
                    let last = me.map(|((repos, id), rev)| (id, repos, rev));
                    self.make_copy(&child, last, op_depth, shadowed)?;
                }
            }
        } else {
            // The depth is taken by a local change of the user: drop the `base-deleted` rows of
            // the parent's layer that would cover it (STMT_DELETE_WORKING_BASE_DELETE and its
            // recursive form).
            for one in [true, false] {
                let which = if one {
                    "local_relpath = ?2".to_owned()
                } else {
                    nodes::strict_descendant("local_relpath", "?2")
                };
                self.conn
                    .execute(
                        &format!(
                            "DELETE FROM nodes WHERE wc_id = ?1 AND {which} \
                             AND presence = 'base-deleted' AND op_depth > ?3 \
                             AND op_depth = (SELECT MIN(op_depth) FROM nodes n \
                             WHERE n.wc_id = ?1 AND n.local_relpath = nodes.local_relpath \
                             AND op_depth > ?3)"
                        ),
                        params![self.wc_id, path.as_str(), last_op_depth],
                    )
                    .map_err(sql_err)?;
            }
        }
        // What the parent's layer has here is not known: `not-present` (done last, as in svn).
        if last_op_depth > 0 && last_op_depth != op_depth {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                     repos_id, repos_path, revision, presence, kind) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'not-present', ?8)",
                    params![
                        self.wc_id,
                        path.as_str(),
                        last_op_depth,
                        parent_text(path),
                        repos_id,
                        repos_relpath,
                        revision,
                        kind
                    ],
                )
                .map_err(sql_err)?;
        }
        Ok(())
    }

    fn row_exists(&self, path: &RelPath, op_depth: u32) -> Result<bool> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                 AND op_depth = ?3",
                params![self.wc_id, path.as_str(), op_depth],
                |r| r.get(0),
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// svn's `db_extend_parent_delete` (see [`ops::extend_parent_delete`]).
    fn extend_parent_delete(&self, path: &RelPath, kind: &str, op_depth: u32) -> Result<()> {
        ops::extend_parent_delete(self.conn, self.wc_id, path, kind, op_depth)
    }

    /// Adds a `base-deleted` working row over the BASE node at `path`: the node exists in the
    /// repository, but what is on disk is not part of it (an obstruction).
    pub fn mark_base_deleted(&self, path: &RelPath, kind: NodeKind) -> Result<()> {
        self.mark_base_deleted_at(path, kind, relpath_depth(path))
    }

    /// Like [`WcTxn::mark_base_deleted`], with the working row at `op_depth`: the nodes below
    /// a shadowed directory (obstructed, or deleted or replaced locally) are covered by the layer
    /// rooted at that directory.
    ///
    /// Like svn's `db_extend_parent_delete`, nothing is written when `path` already has a
    /// working row at `op_depth` or below it: a user's addition or a deeper layer is never
    /// overwritten. This matters when a directory is replaced by a copy: the copied children
    /// sit at the copy's op_depth, the same one the shadowed update drive passes here, and a
    /// server add of a child the copy already has must leave the copied row alone. (A plain
    /// `svn add` below a replaced directory puts children deeper, so they are never at risk.)
    /// Pinned by `incoming_add_into_copy_replaced_dir_keeps_copied_rows_vs_svn` in svn-client.
    pub fn mark_base_deleted_at(
        &self,
        path: &RelPath,
        kind: NodeKind,
        op_depth: u32,
    ) -> Result<()> {
        super::connection::cached_exec(
            self.conn,
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                 presence, kind) SELECT ?1, ?2, ?3, ?4, 'base-deleted', ?5 \
                 WHERE NOT EXISTS (SELECT 1 FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                 AND op_depth > 0 AND op_depth <= ?3)",
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

    /// Sets the working property list of `path` like [`WcDb::set_props`]: the overlay is
    /// dropped when `props` equals the pristine properties.
    pub fn set_props(&self, path: &RelPath, props: &Props) -> Result<()> {
        ops::set_props(self.conn, self.wc_id, path, props)
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

    /// Replaces the conflict record of `path` like [`WcDb::mark_conflict`].
    pub fn write_conflict(&self, path: &RelPath, record: &actual::ConflictRecord) -> Result<()> {
        actual::write_conflict(self.conn, self.wc_id, path, record)
    }

    /// Drops every conflict of `path` like [`WcDb::resolve`].
    pub fn clear_conflict(&self, path: &RelPath) -> Result<()> {
        actual::clear_conflict(self.conn, self.wc_id, path)
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
        ops::queue_work(self.conn, self.wc_id, items)
    }

    /// Completes work items that already ran ([`Unsettled`] from
    /// [`WcDb::run_work_queue_unsettled`]): deletes them and stores their stat caches, like the
    /// runner's own completion (a path that is gone is skipped). The caller makes their files
    /// durable before this transaction commits.
    pub fn complete_work(&self, done: &Unsettled) -> Result<()> {
        for id in &done.ids {
            let id = i64::try_from(*id)
                .map_err(|_| SvnError::new(ErrorCode::Internal, "work item id out of range"))?;
            super::connection::cached_exec(self.conn, "DELETE FROM work_queue WHERE id = ?1", [id])
                .map_err(sql_err)?;
        }
        super::failpoint!("wc.wq.after_delete");
        for r in &done.fileinfo {
            let path = RelPath::new(&r.relpath)?;
            match ops::record_fileinfo(
                self.conn,
                self.wc_id,
                &path,
                Some(r.translated_size),
                Some(r.last_mod_time_micros),
            ) {
                Err(e) if e.code == ErrorCode::PathNotFound => {}
                other => other?,
            }
        }
        super::failpoint!("wc.wq.after_fileinfo");
        Ok(())
    }

    /// Removes finished work items by id (an id that is already gone is fine), like the
    /// deletion step of [`WcDb::wq_record_and_fetch_next`] for many items at once.
    pub fn complete_work_items(&self, ids: &[i64]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("DELETE FROM work_queue WHERE id = ?1")
            .map_err(sql_err)?;
        for id in ids {
            stmt.execute([id]).map_err(sql_err)?;
        }
        Ok(())
    }

    // ---- pristine index -----------------------------------------------------------------

    /// Records the `PRISTINE` row of a text placed with
    /// [`PristineStore::place`](crate::pristine::PristineStore::place); an existing row (and
    /// its refcount) is left alone, like [`PristineIndex::record`](crate::pristine::PristineIndex::record).
    /// Also records a text staged with [`crate::pristine::PristineBatch`]. Must run before the
    /// NODES row that references the text is written, so the refcount trigger counts it.
    ///
    /// Also checks that the stored file exists: an existing row may be unreferenced, and
    /// `svn cleanup` in another process vacuums such rows with their files. Within this write
    /// transaction nobody can remove it any more (svn removes the file inside its own write
    /// transaction, and only while the row is unreferenced), so the check holds until commit.
    /// A missing file fails the transaction (see [`Self::record_pristine_or_restore`]).
    pub fn record_pristine(&self, record: &PristineRecord) -> Result<()> {
        self.record_pristine_or_restore(record, None)
    }

    /// [`Self::record_pristine`] that restores a missing stored file from `spare`, a file
    /// holding exactly the text of `record` (the copy [`Placement::AlreadyStored`] hands back);
    /// see [`crate::pristine::ensure_stored`]. Without a usable spare the transaction fails with
    /// [`ErrorCode::WcBusy`], nothing of it is written, and the node is fetched again by the
    /// next update.
    ///
    /// [`Placement::AlreadyStored`]: crate::pristine::Placement::AlreadyStored
    pub fn record_pristine_or_restore(
        &self,
        record: &PristineRecord,
        spare: Option<&std::path::Path>,
    ) -> Result<()> {
        let size = i64::try_from(record.size)
            .map_err(|_| SvnError::new(ErrorCode::Internal, "pristine size out of range"))?;
        super::connection::cached_exec(
            self.conn,
            "INSERT OR IGNORE INTO pristine (checksum, compression, size, refcount, \
                 md5_checksum) VALUES (?1, NULL, ?2, 0, ?3)",
            params![
                record.sha1.to_svn_string(),
                size,
                record.md5.to_svn_string()
            ],
        )
        .map_err(sql_err)?;
        let adm = self.db.wcroot().join(super::ADM_DIR);
        match crate::pristine::ensure_stored(&adm, record, spare) {
            Ok(_) => Ok(()),
            Err(crate::pristine::PristineError::NotInstalled(sha1)) => Err(SvnError::new(
                ErrorCode::WcBusy,
                format!(
                    "working copy was cleaned up by another client during the update or commit: \
                     pristine text {} was removed; nothing more was written. Run the operation \
                     again to finish it",
                    sha1.to_hex()
                ),
            )),
            Err(e) => Err(e.into()),
        }
    }
}
