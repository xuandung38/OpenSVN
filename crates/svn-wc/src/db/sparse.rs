//! Sparse working copies: removing a node from the BASE tree because its parent's depth no longer
//! covers it (`svn update --set-depth`), or excluding it (`--set-depth exclude`).
//!
//! The row changes follow svn 1.15's `db_base_remove` with `keep_as_working = FALSE`, as called
//! by `crop_children` and `svn_wc_exclude` (`libsvn_wc/crop.c`). Deciding WHAT may be removed
//! (only subtrees without local modifications other than deletes) is the caller's job: it needs
//! the status walk, which lives above this crate's database layer.

use rusqlite::{Connection, params};
use svn_types::{ErrorCode, NodeKind, RelPath, Result, SvnError};

use super::connection::sql_err;
use super::nodes::{self, Presence, kind_word, parent_text, read_base, read_current};
use super::ops::{detach_moves, queue_work, scope_sql};
use crate::workqueue::WorkItem;

/// Work items that remove the working files of a node about to leave the BASE tree.
///
/// Like svn's `db_base_remove`: every node of the subtree whose current row is present gets a
/// `file-remove` or non-recursive `dir-remove` (children first, so an emptied directory goes
/// too), then the node itself. A node whose current row is its own `base-deleted` layer has no
/// working file to remove. Unlike svn, nothing is queued for a node that is `not-present`,
/// `excluded` or `server-excluded`: such a node has no working file, so whatever is on disk at
/// its path belongs to the user.
fn removal_work(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Vec<WorkItem>> {
    let Some(cur) = read_current(conn, wc_id, path)? else {
        return Ok(Vec::new());
    };
    if !cur.presence.is_present() {
        return Ok(Vec::new());
    }
    let mut work = Vec::new();
    if cur.kind == NodeKind::Dir {
        let sql = format!(
            "SELECT local_relpath, kind FROM nodes n WHERE wc_id = ?1 AND {} \
             AND presence IN ('normal', 'incomplete') \
             AND op_depth = (SELECT MAX(op_depth) FROM nodes w WHERE w.wc_id = ?1 \
                             AND w.local_relpath = n.local_relpath) \
             ORDER BY local_relpath DESC",
            nodes::strict_descendant("local_relpath", "?2")
        );
        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params![wc_id, path.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql_err)?;
        for row in rows {
            let (relpath, kind) = row.map_err(sql_err)?;
            work.push(if kind == "dir" {
                WorkItem::DirRemove {
                    relpath,
                    recursive: false,
                }
            } else {
                WorkItem::FileRemove { relpath }
            });
        }
        work.push(WorkItem::DirRemove {
            relpath: path.as_str().to_owned(),
            recursive: false,
        });
    } else {
        work.push(WorkItem::FileRemove {
            relpath: path.as_str().to_owned(),
        });
    }
    Ok(work)
}

/// Removes `path` and everything below it from wc.db (all layers, ACTUAL rows included) and
/// queues the removal of its unmodified working files, in the caller's transaction. With
/// `exclude`, an `excluded` BASE row at the node's repository location and revision takes its
/// place (`svn update --set-depth exclude`). LOCK rows are kept, like svn does.
pub(crate) fn sparse_remove(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    exclude: bool,
) -> Result<()> {
    if path.is_root() {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            "the working copy root cannot be removed from the working copy",
        ));
    }
    let base = read_base(conn, wc_id, path)?.ok_or_else(|| {
        SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{path}' is not in the working copy base"),
        )
    })?;
    let scope = scope_sql();
    // A row of an operation rooted above `path` (a copy or delete of an ancestor) would be cut
    // in half; svn's callers never get here with one, refuse instead of corrupting the layer.
    let foreign: bool = conn
        .query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM nodes WHERE wc_id = ?1 AND {scope} \
                 AND op_depth > 0 AND op_depth < ?3)"
            ),
            params![wc_id, path.as_str(), nodes::relpath_depth(path)],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    if foreign {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("'{path}' is part of a larger local change; commit or revert it first"),
        ));
    }
    let work = removal_work(conn, wc_id, path)?;
    // Moves out of the subtree become copies (the destination is no longer "moved here");
    // moves into it cannot exist, the caller never removes subtrees with additions.
    detach_moves(conn, wc_id, path, 0)?;
    conn.execute(
        &format!("DELETE FROM actual_node WHERE wc_id = ?1 AND {scope}"),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    conn.execute(
        &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope}"),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    if exclude && matches!(base.presence, Presence::Normal | Presence::Incomplete) {
        conn.execute(
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
             repos_path, revision, presence, kind, changed_date) \
             VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, 'excluded', ?7, 0)",
            params![
                wc_id,
                path.as_str(),
                parent_text(path),
                base.repos_id,
                base.repos_relpath,
                base.revision.and_then(|r| i64::try_from(r).ok()),
                kind_word(base.kind),
            ],
        )
        .map_err(sql_err)?;
    }
    let skels: Vec<_> = work.iter().map(WorkItem::to_skel).collect();
    queue_work(conn, &skels)
}
