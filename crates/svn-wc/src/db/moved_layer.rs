//! Rows of a local move's destination after an update moved its source: svn's
//! `replace_moved_layer` (`wc_db_update_move.c`), the "keep as working" copy of a destination
//! node an incoming delete would lose local changes of, and breaking a move.

use rusqlite::params;
use svn_types::{RelPath, Result};

use super::nodes::{relpath_depth, strict_descendant};
use super::ops::{extend_parent_delete, scope_sql};
use super::sql_err;
use super::txn::WcTxn;

/// `(local_relpath, checksum, translated_size, last_mod_time)` of a destination file.
type KnownFile = (String, Option<String>, Option<i64>, Option<i64>);

/// `(CASE ...)`: `local_relpath` (or `parent_relpath`) of a row below `?2` moved below `?3`.
fn mapped(column: &str) -> String {
    format!("(CASE WHEN {column} = ?2 THEN ?3 ELSE ?3 || substr({column}, length(?2) + 1) END)")
}

impl WcTxn<'_> {
    /// svn's `replace_moved_layer`: the destination layer of the move `src` -> `dst` becomes a
    /// copy of the BASE tree now at `src` (`moved_here` rows at the destination's op depth).
    /// Deeper local layers at the destination stay; a node new to the layer below a locally
    /// deleted folder is deleted in that layer too, and a `base-deleted` row left with nothing
    /// below it goes.
    pub fn replace_moved_layer(&self, src: &RelPath, dst: &RelPath) -> Result<()> {
        let conn = self.conn();
        let wc_id = self.wc_id();
        let d = relpath_depth(dst);
        let scope = scope_sql();
        // A file whose text the move does not change keeps what its working file was last
        // known as (svn copies the rows over the destination's).
        let known: Vec<KnownFile> = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT local_relpath, checksum, translated_size, last_mod_time FROM nodes \
                     WHERE wc_id = ?1 AND op_depth = ?3 AND {scope} \
                     AND translated_size IS NOT NULL"
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![wc_id, dst.as_str(), d], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .map_err(sql_err)?;
            rows.collect::<rusqlite::Result<_>>().map_err(sql_err)?
        };
        conn.execute(
            &format!("DELETE FROM nodes WHERE wc_id = ?1 AND op_depth = ?3 AND {scope}"),
            params![wc_id, dst.as_str(), d],
        )
        .map_err(sql_err)?;
        let dst_parent = dst
            .parent()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_default();
        conn.execute(
            &format!(
                "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
                 repos_path, revision, presence, depth, kind, changed_revision, changed_date, \
                 changed_author, checksum, properties, translated_size, last_mod_time, \
                 symlink_target, moved_here) \
                 SELECT wc_id, {}, ?4, CASE WHEN local_relpath = ?2 THEN ?5 ELSE {} END, \
                 repos_id, repos_path, revision, \
                 CASE presence WHEN 'server-excluded' THEN 'not-present' ELSE presence END, \
                 depth, kind, changed_revision, changed_date, changed_author, checksum, \
                 properties, translated_size, last_mod_time, symlink_target, 1 \
                 FROM nodes WHERE wc_id = ?1 AND op_depth = 0 AND file_external IS NULL \
                 AND {scope}",
                mapped("local_relpath"),
                mapped("parent_relpath"),
            ),
            params![wc_id, src.as_str(), dst.as_str(), d, dst_parent],
        )
        .map_err(sql_err)?;
        for (path, checksum, size, mtime) in known {
            conn.execute(
                "UPDATE nodes SET translated_size = ?4, last_mod_time = ?5 WHERE wc_id = ?1 \
                 AND local_relpath = ?2 AND op_depth = ?3 AND checksum IS ?6",
                params![wc_id, path, d, size, mtime, checksum],
            )
            .map_err(sql_err)?;
        }
        // New nodes below a folder deleted (or replaced) inside the destination.
        let added: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT local_relpath, kind FROM nodes WHERE wc_id = ?1 AND op_depth = ?3 \
                     AND presence IN ('normal', 'incomplete') AND {}",
                    strict_descendant("local_relpath", "?2")
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![wc_id, dst.as_str(), d], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(sql_err)?;
            rows.collect::<rusqlite::Result<_>>().map_err(sql_err)?
        };
        for (path, kind) in added {
            let path = super::nodes::rel(path)?;
            extend_parent_delete(conn, wc_id, &path, &kind, d)?;
        }
        conn.execute(
            &format!(
                "DELETE FROM nodes WHERE wc_id = ?1 AND op_depth > ?3 \
                 AND presence = 'base-deleted' AND {scope} AND NOT EXISTS \
                 (SELECT 1 FROM nodes AS lower WHERE lower.wc_id = nodes.wc_id \
                  AND lower.local_relpath = nodes.local_relpath \
                  AND lower.op_depth < nodes.op_depth)"
            ),
            params![wc_id, dst.as_str(), d],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// The destination rows of `node` (and below), at the move's op depth `layer`, copied to a
    /// layer of their own at `node`'s depth (no longer part of the move): what an incoming
    /// delete leaves of a destination node with local changes (svn's "keep as working").
    pub fn keep_moved_as_working(&self, node: &RelPath, layer: u32) -> Result<()> {
        let conn = self.conn();
        let depth = relpath_depth(node);
        conn.execute(
            &format!(
                "INSERT OR IGNORE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                 repos_id, repos_path, revision, presence, depth, kind, changed_revision, \
                 changed_date, changed_author, checksum, properties, translated_size, \
                 last_mod_time, symlink_target) \
                 SELECT wc_id, local_relpath, ?4, parent_relpath, repos_id, repos_path, \
                 revision, presence, depth, kind, changed_revision, changed_date, \
                 changed_author, checksum, properties, translated_size, last_mod_time, \
                 symlink_target FROM nodes WHERE wc_id = ?1 AND op_depth = ?3 AND {}",
                scope_sql()
            ),
            params![self.wc_id(), node.as_str(), layer, depth],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// Breaks the move `src` -> `dst` into a deletion and a copy (svn's
    /// `svn_wc__db_op_break_move_internal`): the source op root forgets its destination and
    /// the destination layer is no longer `moved_here`.
    pub fn break_move(&self, src: &RelPath, dst: &RelPath) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE nodes SET moved_to = NULL WHERE wc_id = ?1 AND local_relpath = ?2 \
             AND op_depth = ?3",
            params![self.wc_id(), src.as_str(), relpath_depth(src)],
        )
        .map_err(sql_err)?;
        conn.execute(
            &format!(
                "UPDATE nodes SET moved_here = NULL WHERE wc_id = ?1 AND op_depth = ?3 AND {}",
                scope_sql()
            ),
            params![self.wc_id(), dst.as_str(), relpath_depth(dst)],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}
