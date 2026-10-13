//! Repository location of a node, like `svn_wc__db_read_repos_info` / `scan_base` /
//! `scan_addition`: where the node lives (or would live) in the repository.

use rusqlite::{Connection, OptionalExtension, params};
use svn_types::{ErrorCode, RelPath, Result, SvnError};

use super::WcDb;
use super::sql_err;

/// Repository location of a working-copy node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReposInfo {
    /// Root URL of the repository.
    pub repos_root_url: String,
    /// Repository UUID.
    pub repos_uuid: String,
    /// Path of the node inside the repository (`""` is the repository root).
    pub repos_relpath: String,
}

/// `(repos_id, repos_relpath)` of `path`.
///
/// * A row that carries a location (BASE node, copy root or any node inside a copy) uses it.
/// * A deleted node reports the location of the layer it deletes (the highest lower row with one).
/// * A locally added node takes its parent's location plus its own name (`scan_addition`), which
///   also covers adds inside copies and adds replacing deleted nodes.
fn location(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<(i64, String)> {
    let current: Option<(i64, Option<i64>, Option<String>, String)> = conn
        .query_row(
            "SELECT op_depth, repos_id, repos_path, presence FROM nodes \
             WHERE wc_id = ?1 AND local_relpath = ?2 ORDER BY op_depth DESC LIMIT 1",
            params![wc_id, path.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((_, repos_id, repos_path, presence)) = current else {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{path}' is not under version control"),
        ));
    };
    if let (Some(id), Some(p)) = (repos_id, repos_path) {
        return Ok((id, p));
    }
    if presence == "base-deleted" {
        let below: Option<(i64, String)> = conn
            .query_row(
                "SELECT repos_id, repos_path FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                 AND repos_id IS NOT NULL AND repos_path IS NOT NULL \
                 ORDER BY op_depth DESC LIMIT 1",
                params![wc_id, path.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(found) = below {
            return Ok(found);
        }
    }
    let Some(parent) = path.parent() else {
        return Err(SvnError::new(
            ErrorCode::Corrupt,
            "wc.db: the working copy root has no repository location",
        ));
    };
    let (id, parent_path) = location(conn, wc_id, &parent)?;
    let name = path.file_name().unwrap_or_default();
    let joined = if parent_path.is_empty() {
        name.to_owned()
    } else {
        format!("{parent_path}/{name}")
    };
    Ok((id, joined))
}

impl WcDb {
    /// Repository root URL, UUID and repository path of `path`, for any versioned node including
    /// added and copied ones. `PathNotFound` when `path` is not versioned.
    pub fn read_repos_info(&self, path: &RelPath) -> Result<ReposInfo> {
        self.with_read(|c| {
            let (id, repos_relpath) = location(c, self.wc_id, path)?;
            let (root, uuid): (String, String) = c
                .query_row(
                    "SELECT root, uuid FROM repository WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(sql_err)?;
            Ok(ReposInfo {
                repos_root_url: root,
                repos_uuid: uuid,
                repos_relpath,
            })
        })
    }
}
