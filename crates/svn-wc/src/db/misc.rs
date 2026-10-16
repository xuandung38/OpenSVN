//! Read side of the BASE caches kept next to the node: the repository LOCK cache, the `dav_cache`
//! blob and the `inherited_props` cache. All three belong to the BASE (`op_depth = 0`) row.
//!
//! The lock cache is written through [`super::WcTxn::lock_add`] and
//! [`super::WcTxn::lock_remove`]; the inherited-properties cache through
//! [`WcDb::set_base_inherited_props`].

use rusqlite::{Connection, OptionalExtension, params};
use svn_types::{RelPath, Result};

use super::WcDb;
use super::nodes::{Props, corrupt, decode_props};
use super::sql_err;
use crate::skel::{self, InheritedProps, Skel};

/// Lock information cached from the server (not authoritative), as in svn's `LOCK` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// Lock token (`opaquelocktoken:...`).
    pub token: String,
    /// Lock owner.
    pub owner: Option<String>,
    /// Lock comment.
    pub comment: Option<String>,
    /// Creation date, microseconds since the epoch (APR time).
    pub date: Option<i64>,
}

fn base_blob(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    column: &str,
) -> Result<Option<Vec<u8>>> {
    super::connection::cached_row(
        conn,
        &format!(
            "SELECT {column} FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0"
        ),
        params![wc_id, path.as_str()],
        |r| r.get::<_, Option<Vec<u8>>>(0),
    )
    .optional()
    .map_err(sql_err)
    .map(Option::flatten)
}

impl WcDb {
    /// Cached repository locks of the BASE nodes at and below `path`, sorted by path, as
    /// `(local relpath, repository relpath, lock)` (`svn_wc__node_get_lock_tokens_recursive`).
    pub fn repos_locks_below(&self, path: &RelPath) -> Result<Vec<(RelPath, String, LockInfo)>> {
        let rows: Vec<(String, String, LockInfo)> = self.with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT n.local_relpath, n.repos_path, l.lock_token, l.lock_owner, \
                     l.lock_comment, l.lock_date \
                     FROM nodes n JOIN lock l ON l.repos_id = n.repos_id \
                     AND l.repos_relpath = n.repos_path \
                     WHERE n.wc_id = ?1 AND n.op_depth = 0 AND (?2 = '' OR n.local_relpath = ?2 \
                     OR substr(n.local_relpath, 1, length(?2) + 1) = ?2 || '/') \
                     ORDER BY n.local_relpath",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![self.wc_id, path.as_str()], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        LockInfo {
                            token: r.get(2)?,
                            owner: r.get(3)?,
                            comment: r.get(4)?,
                            date: r.get(5)?,
                        },
                    ))
                })
                .map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_err)
        })?;
        rows.into_iter()
            .map(|(p, r, l)| {
                let rel = RelPath::new(&p).map_err(|_| corrupt(format!("bad relpath '{p}'")))?;
                Ok((rel, r, l))
            })
            .collect()
    }

    /// Paths at or below `path` that have an `ACTUAL_NODE` row (local property changes, a
    /// conflict or a changelist). A path without one has none of those, so a tree walk can skip
    /// the per-node queries for them.
    pub fn actual_rows_below(&self, path: &RelPath) -> Result<Vec<RelPath>> {
        let rows: Vec<String> = self.with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT local_relpath FROM actual_node WHERE wc_id = ?1 AND (?2 = '' \
                     OR local_relpath = ?2 OR substr(local_relpath, 1, length(?2) + 1) = ?2 || '/')",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![self.wc_id, path.as_str()], |r| r.get(0))
                .map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_err)
        })?;
        rows.into_iter()
            .map(|p| RelPath::new(&p).map_err(|_| corrupt(format!("bad relpath '{p}'"))))
            .collect()
    }

    /// Cached repository lock of the BASE node at `path`, matched by repository location like
    /// svn does (`LOCK.repos_id/repos_relpath` against the node's).
    pub fn repos_lock(&self, path: &RelPath) -> Result<Option<LockInfo>> {
        self.with_read(|c| repos_lock_on(c, self.wc_id, path))
    }

    /// `dav_cache` of the BASE node: the RA layer's property cache (wcprops), decoded as a
    /// property list. `None` when the node has none (always the case for `file://` access).
    pub fn dav_cache(&self, path: &RelPath) -> Result<Option<Props>> {
        self.with_read(|c| {
            base_blob(c, self.wc_id, path, "dav_cache")?
                .map(|b| decode_props(&b))
                .transpose()
        })
    }

    /// BASE nodes at or below `path` whose inherited-properties cache is set, sorted by path
    /// (`svn_wc__get_cached_iprop_children`, before the depth filter).
    pub fn iprop_cached_below(&self, path: &RelPath) -> Result<Vec<RelPath>> {
        let rows: Vec<String> = self.with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT local_relpath FROM nodes WHERE wc_id = ?1 AND op_depth = 0 \
                     AND inherited_props IS NOT NULL AND (?2 = '' OR local_relpath = ?2 \
                     OR substr(local_relpath, 1, length(?2) + 1) = ?2 || '/') \
                     ORDER BY local_relpath",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![self.wc_id, path.as_str()], |r| r.get(0))
                .map_err(sql_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(sql_err)
        })?;
        rows.into_iter()
            .map(|p| RelPath::new(&p).map_err(|_| corrupt(format!("bad relpath '{p}'"))))
            .collect()
    }

    /// Stores (`Some`) or clears (`None`) the inherited-properties cache of the BASE node at
    /// `path`, as svn's update editor does for the working copy root and switched nodes
    /// (`svn_wc__db_base_add_*` with `iprops`). An empty list is stored as `()`: "cached, no
    /// parent has properties". A missing BASE node is not an error (nothing to cache on).
    pub fn set_base_inherited_props(
        &self,
        path: &RelPath,
        iprops: Option<&InheritedProps>,
    ) -> Result<()> {
        let blob = iprops.map(|i| skel::unparse_iprops(i).unparse());
        self.with_write(|tx| {
            tx.execute(
                "UPDATE nodes SET inherited_props = ?3 \
                 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
                params![self.wc_id, path.as_str(), blob],
            )
            .map(|_| ())
            .map_err(sql_err)
        })
    }

    /// Properties inherited from parents outside the working copy, cached on the BASE node:
    /// `(repository path, properties)` from the closest parent outwards as stored. `None` when
    /// the node has no cache.
    pub fn inherited_props(&self, path: &RelPath) -> Result<Option<InheritedProps>> {
        self.with_read(|c| {
            let Some(blob) = base_blob(c, self.wc_id, path, "inherited_props")? else {
                return Ok(None);
            };
            if blob.is_empty() {
                return Ok(Some(Vec::new()));
            }
            skel::parse_iprops(&Skel::parse(&blob)?)
                .map(Some)
                .map_err(|e| corrupt(format!("inherited_props of '{path}': {e}")))
        })
    }
}

/// [`WcDb::repos_lock`] on an open connection.
pub(crate) fn repos_lock_on(
    c: &Connection,
    wc_id: i64,
    path: &RelPath,
) -> Result<Option<LockInfo>> {
    super::connection::cached_row(c,
                "SELECT l.lock_token, l.lock_owner, l.lock_comment, l.lock_date \
                 FROM nodes n JOIN lock l ON l.repos_id = n.repos_id AND l.repos_relpath = n.repos_path \
                 WHERE n.wc_id = ?1 AND n.local_relpath = ?2 AND n.op_depth = 0",
                params![wc_id, path.as_str()],
                |r| {
                    Ok(LockInfo {
                        token: r.get(0)?,
                        owner: r.get(1)?,
                        comment: r.get(2)?,
                        date: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(sql_err)
}
