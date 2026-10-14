//! Read side of the `EXTERNALS` table: file/directory externals recorded for a working copy.
//!
//! The definitions themselves live in `svn:externals` properties; this table records the external
//! working-copy nodes that exist because of them. Writing externals is outside the MVP, except
//! for forgetting them ([`WcDb::forget_externals`]) when their definition leaves the working copy.

use rusqlite::{Row, params};
use svn_types::{NodeKind, RelPath, Result};

use super::WcDb;
use super::nodes::{Presence, corrupt, rel};
use super::sql_err;

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

    /// Externals whose defining directory is `path` or lies below it, sorted by path.
    pub fn externals_defined_at_or_below(&self, path: &RelPath) -> Result<Vec<External>> {
        self.externals_where(
            &format!(
                "(e.def_local_relpath = ?2 OR {})",
                super::nodes::strict_descendant("e.def_local_relpath", "?2")
            ),
            path,
        )
    }

    /// Forgets the externals at `paths`: their `EXTERNALS` rows and, for a file external, its
    /// BASE row and actual properties. Nothing on disk is touched, so what an external left
    /// there becomes unversioned, as after svn removes an external whose definition is gone.
    /// A file external with local operations on it (rows above BASE) is left as it is.
    pub fn forget_externals(&self, paths: &[RelPath]) -> Result<()> {
        self.with_write(|c| {
            for p in paths {
                let worked: bool = c
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM nodes WHERE wc_id = ?1 \
                         AND local_relpath = ?2 AND op_depth > 0)",
                        params![self.wc_id, p.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(sql_err)?;
                let file_external = c
                    .execute(
                        "DELETE FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
                         AND op_depth = 0 AND file_external IS NOT NULL AND NOT ?3",
                        params![self.wc_id, p.as_str(), worked],
                    )
                    .map_err(sql_err)?
                    > 0;
                if file_external {
                    c.execute(
                        "DELETE FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2",
                        params![self.wc_id, p.as_str()],
                    )
                    .map_err(sql_err)?;
                }
                let is_file_node: bool = c
                    .query_row(
                        "SELECT EXISTS (SELECT 1 FROM nodes WHERE wc_id = ?1 \
                         AND local_relpath = ?2 AND file_external IS NOT NULL)",
                        params![self.wc_id, p.as_str()],
                        |r| r.get(0),
                    )
                    .map_err(sql_err)?;
                if !is_file_node {
                    c.execute(
                        "DELETE FROM externals WHERE wc_id = ?1 AND local_relpath = ?2",
                        params![self.wc_id, p.as_str()],
                    )
                    .map_err(sql_err)?;
                }
            }
            Ok(())
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
