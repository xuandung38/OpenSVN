//! Recording a move that happened outside version control (TortoiseSVN's "Repair move").
//!
//! TortoiseSVN renames the destination back to the source, reverts what svn recorded for the two
//! paths (a scheduled delete of the source, a plain add of the destination) and runs
//! `svn move`. [`op_mark_move`] writes the rows that sequence leaves behind without touching the
//! disk: the same rows `svn move` (svn 1.15 `svn_wc__move2`, metadata only) writes, so the move
//! commits as a move and the destination keeps its current content.

use rusqlite::{Connection, params};
use svn_types::{ErrorCode, RelPath, Result, SvnError};

use super::connection::sql_err;
use super::nodes::{Presence, read_base, read_current, relpath_depth, strict_descendant};
use super::ops::{op_copy, op_delete, op_revert, scope_sql};

/// What [`super::WcDb::op_mark_move`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkMove {
    /// The pair was already recorded as this move; nothing changed.
    AlreadyMoved,
    /// Recorded as a move (`moved_to` / `moved_here`).
    Moved,
    /// The source is a mixed-revision directory: recorded as a copy plus a delete without move
    /// bookkeeping, like `svn move --allow-mixed-revisions` does.
    CopiedAndDeleted,
}

fn invalid(msg: String) -> SvnError {
    SvnError::new(ErrorCode::WcInvalidOperation, msg)
}

fn exists(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<bool> {
    conn.query_row(&format!("SELECT EXISTS ({sql})"), p, |r| r.get(0))
        .map_err(sql_err)
}

/// Whether the BASE subtree at `path` spans more than one revision (svn's
/// `svn_wc__db_min_max_revisions`, used by `svn move` to refuse or downgrade a move).
fn is_mixed_revision(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<bool> {
    conn.query_row(
        &format!(
            "SELECT MIN(revision) <> MAX(revision) FROM nodes WHERE wc_id = ?1 AND {} \
             AND presence IN ('normal', 'incomplete') AND file_external IS NULL AND op_depth = 0",
            scope_sql()
        ),
        params![wc_id, path.as_str()],
        |r| r.get::<_, Option<bool>>(0),
    )
    .map(|v| v.unwrap_or(false))
    .map_err(sql_err)
}

fn has_conflicts(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<bool> {
    exists(
        conn,
        &format!(
            "SELECT 1 FROM actual_node WHERE wc_id = ?1 AND {} \
             AND (conflict_data IS NOT NULL OR tree_conflict_data IS NOT NULL)",
            scope_sql()
        ),
        params![wc_id, path.as_str()],
    )
}

/// Records that `src` (missing from disk, or scheduled for deletion) was moved to `dst` (an
/// unversioned item, a plain addition, or a copy of `src`), in the caller's transaction.
///
/// The caller checks the disk side: `src` absent unless it is scheduled for deletion, `dst`
/// present with the node kind of `src`. Refused with [`ErrorCode::WcInvalidOperation`]: a source
/// without a committed BASE node, replaced or deleted as part of its parent, or already moved
/// elsewhere; a destination that is versioned in another way (BASE node, copy of something else,
/// addition containing copies); conflicts on either side.
pub(crate) fn op_mark_move(
    conn: &Connection,
    wc_id: i64,
    src: &RelPath,
    dst: &RelPath,
) -> Result<MarkMove> {
    if src.is_root() || dst.is_root() {
        return Err(invalid(
            "the working copy root cannot be part of a move".to_owned(),
        ));
    }
    if src == dst || dst.starts_with(src) || src.starts_with(dst) {
        return Err(invalid(format!(
            "cannot record a move between '{src}' and '{dst}': one contains the other"
        )));
    }
    let (ds, dd) = (relpath_depth(src), relpath_depth(dst));
    let base = read_base(conn, wc_id, src)?
        .filter(|b| b.presence == Presence::Normal)
        .ok_or_else(|| {
            invalid(format!(
                "'{src}' has no committed version; only committed items can be moved"
            ))
        })?;
    let cur = read_current(conn, wc_id, src)?.unwrap_or_else(|| base.clone());
    let src_deleted = if cur.op_depth == 0 {
        false
    } else if cur.presence == Presence::BaseDeleted && cur.op_depth == ds {
        match &cur.moved_to {
            Some(to) if to == dst => return Ok(MarkMove::AlreadyMoved),
            Some(to) => {
                return Err(invalid(format!("'{src}' is already moved to '{to}'")));
            }
            None => true,
        }
    } else if cur.presence == Presence::BaseDeleted {
        return Err(invalid(format!(
            "'{src}' is deleted together with its parent; record the move of the parent instead"
        )));
    } else {
        return Err(invalid(format!(
            "'{src}' is replaced; revert it before recording a move"
        )));
    };
    if has_conflicts(conn, wc_id, src)? || has_conflicts(conn, wc_id, dst)? {
        return Err(invalid(format!(
            "'{src}' or '{dst}' has a conflict; resolve it first"
        )));
    }

    // What the destination is now.
    let dcur = read_current(conn, wc_id, dst)?;
    let mut revert_dst = false;
    let mut flag_copy = false;
    match &dcur {
        None => {}
        Some(n) if matches!(n.presence, Presence::NotPresent | Presence::BaseDeleted) => {}
        Some(n) if n.presence.is_present() && n.op_depth == dd => {
            if n.repos_id.is_none() {
                // A plain addition, e.g. the renamed item added by hand. It must not replace a
                // BASE node or hide copies that a revert would throw away.
                let replaces = exists(
                    conn,
                    "SELECT 1 FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                     AND op_depth < ?3 AND presence IN ('normal', 'incomplete')",
                    params![wc_id, dst.as_str(), dd],
                )?;
                let copies_inside = exists(
                    conn,
                    &format!(
                        "SELECT 1 FROM nodes WHERE wc_id = ?1 AND {} AND op_depth > 0 \
                         AND repos_id IS NOT NULL",
                        scope_sql()
                    ),
                    params![wc_id, dst.as_str()],
                )?;
                if replaces || copies_inside {
                    return Err(invalid(format!(
                        "'{dst}' is already under version control in another way; revert it first"
                    )));
                }
                revert_dst = true;
            } else if !n.moved_here
                && n.repos_id == base.repos_id
                && n.repos_relpath == base.repos_relpath
                && n.revision == base.revision
            {
                flag_copy = true; // `svn copy src dst` + `svn delete src`
            } else {
                return Err(invalid(format!(
                    "'{dst}' is already under version control as a copy of another item"
                )));
            }
        }
        Some(n) if n.presence.is_present() => {
            return Err(invalid(format!("'{dst}' is already under version control")));
        }
        Some(n) => {
            return Err(invalid(format!(
                "'{dst}' is {} in this working copy",
                n.presence.as_str()
            )));
        }
    }

    let record = !is_mixed_revision(conn, wc_id, src)?;
    if flag_copy {
        if !src_deleted {
            op_delete(conn, wc_id, src)?;
        }
    } else {
        if src_deleted {
            op_revert(conn, wc_id, src, true)?;
        }
        if revert_dst {
            op_revert(conn, wc_id, dst, true)?;
        }
        op_copy(conn, wc_id, src, dst, record)?;
        op_delete(conn, wc_id, src)?;
    }
    if !record {
        return Ok(MarkMove::CopiedAndDeleted);
    }
    conn.execute(
        "UPDATE nodes SET moved_to = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = ?4 \
         AND presence = 'base-deleted'",
        params![wc_id, src.as_str(), dst.as_str(), ds],
    )
    .map_err(sql_err)?;
    if flag_copy {
        conn.execute(
            &format!(
                "UPDATE nodes SET moved_here = 1 WHERE wc_id = ?1 AND op_depth = ?3 \
                 AND (local_relpath = ?2 OR {}) AND presence NOT IN ('not-present', 'base-deleted')",
                strict_descendant("local_relpath", "?2")
            ),
            params![wc_id, dst.as_str(), dd],
        )
        .map_err(sql_err)?;
    }
    Ok(MarkMove::Moved)
}
