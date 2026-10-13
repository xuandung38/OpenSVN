//! ACTUAL_NODE: local property changes, changelists and conflict records.
//!
//! A row exists only while at least one of its columns carries information; every writer here
//! prunes the row once it is empty, like svn.

use rusqlite::{Connection, OptionalExtension, params};
use svn_types::{RelPath, Result};

use super::connection::sql_err;
use super::nodes::{Props, decode_props, encode_props, parent_text};
use crate::skel::Skel;

/// Checksums of the three conflict texts kept in the pristine store (older/left/right).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictTexts {
    /// Common ancestor text checksum.
    pub older: Option<String>,
    /// Left (base) text checksum.
    pub left: Option<String>,
    /// Right (incoming) text checksum.
    pub right: Option<String>,
}

/// Conflict information of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRecord {
    /// The `conflict_data` skel.
    pub skel: Skel,
    /// Pristine checksums referenced by the conflict.
    pub texts: ConflictTexts,
}

const ALL_EMPTY: &str = "properties IS NULL AND conflict_old IS NULL AND conflict_new IS NULL \
    AND conflict_working IS NULL AND prop_reject IS NULL AND changelist IS NULL \
    AND text_mod IS NULL AND tree_conflict_data IS NULL AND conflict_data IS NULL \
    AND older_checksum IS NULL AND left_checksum IS NULL AND right_checksum IS NULL";

fn ensure_row(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO actual_node (wc_id, local_relpath, parent_relpath) \
         VALUES (?1, ?2, ?3)",
        params![wc_id, path.as_str(), parent_text(path)],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Deletes the row of `path` when nothing is recorded in it any more.
pub(crate) fn prune(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<()> {
    conn.execute(
        &format!("DELETE FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2 AND {ALL_EMPTY}"),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Locally modified properties, or `None` when the row holds no property change.
pub(crate) fn read_props(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Option<Props>> {
    let blob: Option<Option<Vec<u8>>> = conn
        .query_row(
            "SELECT properties FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2",
            params![wc_id, path.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    blob.flatten().as_deref().map(decode_props).transpose()
}

/// Stores `props` as the working properties; `None` (or a set equal to the pristine ones, which
/// the caller decides) clears the column.
pub(crate) fn write_props(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    props: Option<&Props>,
) -> Result<()> {
    match props {
        Some(p) => {
            ensure_row(conn, wc_id, path)?;
            conn.execute(
                "UPDATE actual_node SET properties = ?3 WHERE wc_id = ?1 AND local_relpath = ?2",
                params![wc_id, path.as_str(), encode_props(p)],
            )
            .map_err(sql_err)?;
        }
        None => {
            conn.execute(
                "UPDATE actual_node SET properties = NULL WHERE wc_id = ?1 AND local_relpath = ?2",
                params![wc_id, path.as_str()],
            )
            .map_err(sql_err)?;
            prune(conn, wc_id, path)?;
        }
    }
    Ok(())
}

pub(crate) fn read_changelist(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
) -> Result<Option<String>> {
    let v: Option<Option<String>> = conn
        .query_row(
            "SELECT changelist FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2",
            params![wc_id, path.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(v.flatten())
}

pub(crate) fn write_changelist(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    name: Option<&str>,
) -> Result<()> {
    if name.is_some() {
        ensure_row(conn, wc_id, path)?;
    }
    conn.execute(
        "UPDATE actual_node SET changelist = ?3 WHERE wc_id = ?1 AND local_relpath = ?2",
        params![wc_id, path.as_str(), name],
    )
    .map_err(sql_err)?;
    prune(conn, wc_id, path)
}

pub(crate) fn read_conflict(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
) -> Result<Option<ConflictRecord>> {
    type Cols = (
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<Cols> = conn
        .query_row(
            "SELECT conflict_data, older_checksum, left_checksum, right_checksum \
             FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2",
            params![wc_id, path.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_err)?;
    match row {
        Some((Some(blob), older, left, right)) => Ok(Some(ConflictRecord {
            skel: Skel::parse(&blob)?,
            texts: ConflictTexts { older, left, right },
        })),
        _ => Ok(None),
    }
}

pub(crate) fn write_conflict(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    record: &ConflictRecord,
) -> Result<()> {
    ensure_row(conn, wc_id, path)?;
    conn.execute(
        "UPDATE actual_node SET conflict_data = ?3, older_checksum = ?4, left_checksum = ?5, \
         right_checksum = ?6 WHERE wc_id = ?1 AND local_relpath = ?2",
        params![
            wc_id,
            path.as_str(),
            record.skel.unparse(),
            record.texts.older,
            record.texts.left,
            record.texts.right
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Stores only the `conflict_data` skel of `path`, keeping the checksum columns.
pub(crate) fn write_conflict_data(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    skel: &Skel,
) -> Result<()> {
    ensure_row(conn, wc_id, path)?;
    conn.execute(
        "UPDATE actual_node SET conflict_data = ?3 WHERE wc_id = ?1 AND local_relpath = ?2",
        params![wc_id, path.as_str(), skel.unparse()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Clears every conflict column of `path`.
pub(crate) fn clear_conflict(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<()> {
    conn.execute(
        "UPDATE actual_node SET conflict_data = NULL, older_checksum = NULL, left_checksum = NULL, \
         right_checksum = NULL, conflict_old = NULL, conflict_new = NULL, conflict_working = NULL, \
         prop_reject = NULL, tree_conflict_data = NULL WHERE wc_id = ?1 AND local_relpath = ?2",
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    prune(conn, wc_id, path)
}

/// Drops local modifications (properties, conflicts, text_mod) of `path` and, with
/// `recursive`, of its subtree. Changelist membership is kept, like `svn revert`.
pub(crate) fn clear_modifications(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    recursive: bool,
) -> Result<()> {
    let sub = super::nodes::strict_descendant("local_relpath", "?2");
    let scope = if recursive {
        format!("(local_relpath = ?2 OR {sub})")
    } else {
        "local_relpath = ?2".to_owned()
    };
    conn.execute(
        &format!(
            "UPDATE actual_node SET properties = NULL, text_mod = NULL, conflict_data = NULL, \
             older_checksum = NULL, left_checksum = NULL, right_checksum = NULL, \
             conflict_old = NULL, conflict_new = NULL, conflict_working = NULL, \
             prop_reject = NULL, tree_conflict_data = NULL WHERE wc_id = ?1 AND {scope}"
        ),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    conn.execute(
        &format!("DELETE FROM actual_node WHERE wc_id = ?1 AND {scope} AND {ALL_EMPTY}"),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Removes ACTUAL rows under `path` (inclusive) whose NODES rows are all gone and that carry no
/// conflict.
pub(crate) fn drop_orphans(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<()> {
    let sub = super::nodes::strict_descendant("local_relpath", "?2");
    conn.execute(
        &format!(
            "DELETE FROM actual_node WHERE wc_id = ?1 AND (local_relpath = ?2 OR {sub}) \
             AND conflict_data IS NULL AND tree_conflict_data IS NULL \
             AND NOT EXISTS (SELECT 1 FROM nodes n WHERE n.wc_id = actual_node.wc_id \
                             AND n.local_relpath = actual_node.local_relpath)"
        ),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    Ok(())
}
