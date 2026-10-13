//! Read side of the BASE caches kept next to the node: the repository LOCK cache, the `dav_cache`
//! blob and the `inherited_props` cache. All three belong to the BASE (`op_depth = 0`) row.
//!
//! Writing these caches is not needed by the MVP (the lock cache is written by the lock commands
//! through a later milestone), so only readers exist here.

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
    conn.query_row(
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
    /// Cached repository lock of the BASE node at `path`, matched by repository location like
    /// svn does (`LOCK.repos_id/repos_relpath` against the node's).
    pub fn lock(&self, path: &RelPath) -> Result<Option<LockInfo>> {
        self.with_read(|c| {
            c.query_row(
                "SELECT l.lock_token, l.lock_owner, l.lock_comment, l.lock_date \
                 FROM nodes n JOIN lock l ON l.repos_id = n.repos_id AND l.repos_relpath = n.repos_path \
                 WHERE n.wc_id = ?1 AND n.local_relpath = ?2 AND n.op_depth = 0",
                params![self.wc_id, path.as_str()],
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
        })
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
