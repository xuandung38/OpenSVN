//! Read side of the `EXTERNALS` table: file/directory externals recorded for a working copy.
//!
//! The definitions themselves live in `svn:externals` properties; this table records the external
//! working-copy nodes that exist because of them. [`WcDb::externals_definitions_under`] reads the
//! definitions the way svn gathers them before it processes externals; the writers on
//! [`WcTxn`] record, mark and drop externals inside the caller's transaction.

use rusqlite::{Row, params};
use svn_types::{Depth, ErrorCode, NodeKind, RelPath, Result, SvnError};

use super::nodes::{Presence, corrupt, decode_props, kind_word, parent_text, rel};
use super::ops::ensure_repository;
use super::txn::WcTxn;
use super::{WcDb, sql_err};

/// One `EXTERNALS` row joined with its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct External {
    /// Where the external lives in the working copy.
    pub local_relpath: RelPath,
    /// Parent directory of `local_relpath`.
    pub parent_relpath: RelPath,
    /// Root URL of the external's repository.
    pub repos_root: String,
    /// UUID of the external's repository.
    pub repos_uuid: String,
    /// [`Presence::Normal`] or [`Presence::Excluded`].
    pub presence: Presence,
    /// File or directory.
    pub kind: NodeKind,
    /// Directory whose `svn:externals` property defines this external.
    pub def_local_relpath: RelPath,
    /// Repository relative path as written in the definition.
    pub def_repos_relpath: String,
    /// Operational (peg) revision when the definition pins one.
    pub def_operational_revision: Option<u64>,
    /// Node revision when the definition pins one.
    pub def_revision: Option<u64>,
}

const COLUMNS: &str = "e.local_relpath, e.parent_relpath, r.root, r.uuid, e.presence, e.kind, \
    e.def_local_relpath, e.def_repos_relpath, e.def_operational_revision, e.def_revision";

/// Parses a stored revision (svn writes the decimal text of the revision number).
fn revision(text: Option<String>) -> Result<Option<u64>> {
    text.map(|t| {
        t.parse::<u64>()
            .map_err(|_| corrupt(format!("external revision {t:?} is not a number")))
    })
    .transpose()
}

fn from_row(r: &Row<'_>) -> Result<External> {
    let g = sql_err;
    let presence = Presence::parse(&r.get::<_, String>(4).map_err(g)?)?;
    if !matches!(presence, Presence::Normal | Presence::Excluded) {
        return Err(corrupt("external with presence other than normal/excluded"));
    }
    let kind = match r.get::<_, String>(5).map_err(g)?.as_str() {
        "file" => NodeKind::File,
        "dir" => NodeKind::Dir,
        // Written by the 1.6 -> 1.7 upgrade and by `svn_wc__db_external_add_symlink`.
        "symlink" => NodeKind::Symlink,
        "unknown" => NodeKind::Unknown,
        other => return Err(corrupt(format!("unknown external kind {other:?}"))),
    };
    Ok(External {
        local_relpath: rel(r.get(0).map_err(g)?)?,
        parent_relpath: rel(r.get(1).map_err(g)?)?,
        repos_root: r.get(2).map_err(g)?,
        repos_uuid: r.get(3).map_err(g)?,
        presence,
        kind,
        def_local_relpath: rel(r.get(6).map_err(g)?)?,
        def_repos_relpath: r.get(7).map_err(g)?,
        def_operational_revision: revision(r.get(8).map_err(g)?)?,
        def_revision: revision(r.get(9).map_err(g)?)?,
    })
}

impl WcDb {
    /// The external recorded at exactly `path`, if any.
    pub fn external(&self, path: &RelPath) -> Result<Option<External>> {
        Ok(self
            .externals_where("e.local_relpath = ?2", path)?
            .into_iter()
            .next())
    }

    /// Externals defined by the `svn:externals` property of directory `dir`, sorted by path.
    pub fn externals_defined_in(&self, dir: &RelPath) -> Result<Vec<External>> {
        self.externals_where("e.def_local_relpath = ?2", dir)
    }

    /// Every recorded external at or below `path`, sorted by path (for status / update walks).
    pub fn externals_under(&self, path: &RelPath) -> Result<Vec<External>> {
        self.externals_where(
            &format!(
                "(e.local_relpath = ?2 OR {})",
                super::nodes::strict_descendant("e.local_relpath", "?2")
            ),
            path,
        )
    }

    /// The `svn:externals` values of the versioned directories at or below `path`, with their
    /// recorded depth, sorted by path. Like svn's `svn_wc__externals_gather_definitions`, the
    /// local (ACTUAL) value wins over the committed one, and only present directories
    /// (`normal` or `incomplete` in their topmost layer) count.
    pub fn externals_definitions_under(
        &self,
        path: &RelPath,
    ) -> Result<Vec<(RelPath, Vec<u8>, Depth)>> {
        self.with_read(|c| {
            let sql = format!(
                "SELECT n.local_relpath, \
                   IFNULL((SELECT a.properties FROM actual_node a WHERE a.wc_id = n.wc_id \
                     AND a.local_relpath = n.local_relpath), n.properties), n.depth \
                 FROM nodes n WHERE n.wc_id = ?1 \
                   AND (n.local_relpath = ?2 OR {}) \
                   AND n.op_depth = (SELECT MAX(op_depth) FROM nodes m WHERE m.wc_id = n.wc_id \
                     AND m.local_relpath = n.local_relpath) \
                   AND n.kind = 'dir' AND n.presence IN ('normal', 'incomplete') \
                 ORDER BY n.local_relpath",
                super::nodes::strict_descendant("n.local_relpath", "?2")
            );
            let mut stmt = c.prepare(&sql).map_err(sql_err)?;
            let mut rows = stmt
                .query(params![self.wc_id, path.as_str()])
                .map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().map_err(sql_err)? {
                let blob: Option<Vec<u8>> = r.get(1).map_err(sql_err)?;
                let props = decode_props(blob.as_deref().unwrap_or_default())?;
                let Some((_, value)) = props.into_iter().find(|(k, _)| k == b"svn:externals")
                else {
                    continue;
                };
                let depth: Option<String> = r.get(2).map_err(sql_err)?;
                let depth = match depth.as_deref() {
                    Some("empty") => Depth::Empty,
                    Some("files") => Depth::Files,
                    Some("immediates") => Depth::Immediates,
                    _ => Depth::Infinity,
                };
                out.push((rel(r.get(0).map_err(sql_err)?)?, value, depth));
            }
            Ok(out)
        })
    }

    fn externals_where(&self, cond: &str, arg: &RelPath) -> Result<Vec<External>> {
        self.with_read(|c| {
            let sql = format!(
                "SELECT {COLUMNS} FROM externals e JOIN repository r ON r.id = e.repos_id \
                 WHERE e.wc_id = ?1 AND {cond} ORDER BY e.local_relpath"
            );
            let mut stmt = c.prepare(&sql).map_err(sql_err)?;
            let mut rows = stmt
                .query(params![self.wc_id, arg.as_str()])
                .map_err(sql_err)?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().map_err(sql_err)? {
                out.push(from_row(r)?);
            }
            Ok(out)
        })
    }
}

/// An `EXTERNALS` row to record (see [`WcTxn::external_add`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewExternal {
    /// File or directory.
    pub kind: NodeKind,
    /// Root URL of the external's repository (as wc.db stores it, no trailing slash).
    pub repos_root: String,
    /// UUID of the external's repository.
    pub repos_uuid: String,
    /// Directory whose `svn:externals` property defines the external.
    pub def_local_relpath: RelPath,
    /// Repository relative path of the definition's URL.
    pub def_repos_relpath: String,
    /// Peg revision when the definition pins one.
    pub def_operational_revision: Option<u64>,
    /// Operative revision when the definition pins one.
    pub def_revision: Option<u64>,
}

impl WcTxn<'_> {
    /// Records (or replaces) the external at `path`, as `svn_wc__external_register` and
    /// `svn_wc__db_external_add_file` write it: presence `normal`, the definition's location
    /// and its pinned revisions as decimal text.
    pub fn external_add(&self, path: &RelPath, ext: &NewExternal) -> Result<()> {
        let parent = parent_text(path).ok_or_else(|| {
            SvnError::new(
                ErrorCode::Internal,
                "the working copy root cannot be an external of itself",
            )
        })?;
        let repos_id = ensure_repository(self.conn(), &ext.repos_root, &ext.repos_uuid)?;
        self.conn()
            .execute(
                "INSERT OR REPLACE INTO externals (wc_id, local_relpath, parent_relpath, \
                 repos_id, presence, kind, def_local_relpath, def_repos_relpath, \
                 def_operational_revision, def_revision) \
                 VALUES (?1, ?2, ?3, ?4, 'normal', ?5, ?6, ?7, ?8, ?9)",
                params![
                    self.wc_id(),
                    path.as_str(),
                    parent,
                    repos_id,
                    kind_word(ext.kind),
                    ext.def_local_relpath.as_str(),
                    ext.def_repos_relpath,
                    ext.def_operational_revision.map(|r| r.to_string()),
                    ext.def_revision.map(|r| r.to_string()),
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Drops the `EXTERNALS` row of `path` (nothing else).
    pub fn external_remove(&self, path: &RelPath) -> Result<()> {
        self.conn()
            .execute(
                "DELETE FROM externals WHERE wc_id = ?1 AND local_relpath = ?2",
                params![self.wc_id(), path.as_str()],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Marks the BASE file `path` as a file external, the way svn records one: `file_external`
    /// set and an empty inherited-properties cache (a file external is an update root).
    pub fn mark_file_external(&self, path: &RelPath) -> Result<()> {
        let n = self
            .conn()
            .execute(
                "UPDATE nodes SET file_external = 1, \
                   inherited_props = IFNULL(inherited_props, CAST('()' AS BLOB)) \
                 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0 AND kind = 'file'",
                params![self.wc_id(), path.as_str()],
            )
            .map_err(sql_err)?;
        if n == 0 {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("'{path}' has no BASE file to mark as a file external"),
            ));
        }
        Ok(())
    }
}
