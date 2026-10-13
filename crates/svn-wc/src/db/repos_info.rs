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
    /// Path of the node inside the repository (`""` is the repository root); for added and
    /// copied nodes the path they will have once committed.
    pub repos_relpath: String,
    /// Where a copied node comes from, `(repository path, revision)`; `None` unless the node's
    /// own current row is a copy (copy root or node inside a copy).
    pub copyfrom: Option<(String, u64)>,
}

/// `(repos_id, repos_relpath)` where `path` lives or would live in the repository.
///
/// * A BASE node uses its own location.
/// * A deleted node reports the BASE location it deletes (parent location plus name when the
///   deletion sits inside a copy and there is no BASE row).
/// * Added and copied nodes (any working layer) take the location they will have when committed:
///   their parent's location plus their name, like `scan_addition`. The copy source is reported
///   separately in [`ReposInfo::copyfrom`].
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
    let Some((op_depth, repos_id, repos_path, presence)) = current else {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{path}' is not under version control"),
        ));
    };
    if op_depth == 0
        && let (Some(id), Some(p)) = (repos_id, repos_path)
    {
        return Ok((id, p));
    }
    if presence == "base-deleted" {
        let base: Option<(i64, String)> = conn
            .query_row(
                "SELECT repos_id, repos_path FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                 AND op_depth = 0 AND repos_id IS NOT NULL AND repos_path IS NOT NULL",
                params![wc_id, path.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some(found) = base {
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
            let copyfrom: Option<(String, i64)> = c
                .query_row(
                    "SELECT repos_path, revision FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                     AND op_depth > 0 AND repos_path IS NOT NULL AND revision IS NOT NULL \
                     AND op_depth = (SELECT MAX(op_depth) FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2)",
                    params![self.wc_id, path.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql_err)?;
            Ok(ReposInfo {
                repos_root_url: root,
                repos_uuid: uuid,
                repos_relpath,
                copyfrom: copyfrom.and_then(|(p, r)| u64::try_from(r).ok().map(|r| (p, r))),
            })
        })
    }
}
