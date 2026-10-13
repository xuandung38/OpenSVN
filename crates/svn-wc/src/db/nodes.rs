//! NODES model: layered rows keyed by `(wc_id, local_relpath, op_depth)` and the read side of it.
//!
//! `op_depth = 0` is the BASE tree (checkout/update/commit), `op_depth > 0` are working layers
//! created by add/delete/copy/move; the row with the highest `op_depth` of a path is its current
//! state (the `NODES_CURRENT` view). Writes live in [`super::ops`].

use rusqlite::{Connection, OptionalExtension, Row, params};
use svn_types::{Depth, ErrorCode, NodeKind, RelPath, Result, SvnError};

use super::connection::sql_err;
use crate::skel::{self, PropPair, Skel};

/// Property list as stored in skels: `(name, value)` byte pairs.
pub type Props = Vec<PropPair>;

/// `presence` column values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Ordinary node (BASE) or added/copied node (working layer).
    Normal,
    /// Excluded by the server (authz).
    ServerExcluded,
    /// Administratively excluded (sparse working copy).
    Excluded,
    /// Known not to exist at this revision.
    NotPresent,
    /// Information not complete yet (interrupted update).
    Incomplete,
    /// Working layer recording a delete of the node below.
    BaseDeleted,
}

impl Presence {
    /// Word stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Presence::Normal => "normal",
            Presence::ServerExcluded => "server-excluded",
            Presence::Excluded => "excluded",
            Presence::NotPresent => "not-present",
            Presence::Incomplete => "incomplete",
            Presence::BaseDeleted => "base-deleted",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "normal" => Presence::Normal,
            "server-excluded" => Presence::ServerExcluded,
            "excluded" => Presence::Excluded,
            "not-present" => Presence::NotPresent,
            "incomplete" => Presence::Incomplete,
            "base-deleted" => Presence::BaseDeleted,
            // svn < 1.7 final used "absent"; format 30 renamed it.
            other => return Err(corrupt(format!("unknown presence {other:?}"))),
        })
    }

    /// True for presences that describe an existing, versioned node.
    pub fn is_present(self) -> bool {
        matches!(self, Presence::Normal | Presence::Incomplete)
    }
}

/// One NODES row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Working copy relative path.
    pub relpath: RelPath,
    /// Layer; 0 is BASE.
    pub op_depth: u32,
    /// Presence in this layer.
    pub presence: Presence,
    /// Node kind (`unknown` is reported as [`NodeKind::None`]).
    pub kind: NodeKind,
    /// `REPOSITORY.id` of the repository location (BASE) or copy source (working layers).
    pub repos_id: Option<i64>,
    /// Repository relative path matching `repos_id`.
    pub repos_relpath: Option<String>,
    /// Revision matching `repos_id`.
    pub revision: Option<u64>,
    /// Directory depth (BASE directories); `None` means default.
    pub depth: Option<Depth>,
    /// Pristine SHA-1 as stored, e.g. `$sha1$<hex>`.
    pub checksum: Option<String>,
    /// Symlink target.
    pub symlink_target: Option<String>,
    /// Last changed revision.
    pub changed_rev: Option<u64>,
    /// Last changed date, microseconds since the epoch (APR time).
    pub changed_date: Option<i64>,
    /// Last changed author.
    pub changed_author: Option<String>,
    /// Size of the working file when it was last known unmodified.
    pub translated_size: Option<i64>,
    /// Mtime (APR time) of the working file when it was last known unmodified.
    pub last_mod_time: Option<i64>,
    /// Pristine properties (`None` when the layer has no property information).
    pub properties: Option<Props>,
    /// Node was moved here (rather than plain copied).
    pub moved_here: bool,
    /// Destination of a move of the node below this delete layer.
    pub moved_to: Option<RelPath>,
    /// A file external occupies this location.
    pub file_external: bool,
}

pub(crate) fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, format!("wc.db: {}", msg.into()))
}

/// Parses the stored relpath text, which is already normalized by whoever wrote it.
pub(crate) fn rel(text: String) -> Result<RelPath> {
    RelPath::new(&text).map_err(|e| corrupt(format!("bad relpath {text:?}: {e}")))
}

pub(crate) fn kind_word(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "file",
        NodeKind::Dir => "dir",
        NodeKind::Symlink => "symlink",
        _ => "unknown",
    }
}

fn parse_kind(s: &str) -> NodeKind {
    match s {
        "file" => NodeKind::File,
        "dir" => NodeKind::Dir,
        "symlink" => NodeKind::Symlink,
        _ => NodeKind::None,
    }
}

pub(crate) fn depth_word(depth: Depth) -> &'static str {
    match depth {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
        Depth::Exclude => "exclude",
        Depth::Unknown => "unknown",
    }
}

fn parse_depth(s: &str) -> Depth {
    match s {
        "empty" => Depth::Empty,
        "files" => Depth::Files,
        "immediates" => Depth::Immediates,
        "infinity" => Depth::Infinity,
        "exclude" => Depth::Exclude,
        _ => Depth::Unknown,
    }
}

/// Decodes a `properties` blob; an empty blob or `()` is an empty property list.
pub(crate) fn decode_props(blob: &[u8]) -> Result<Props> {
    // svn reads a zero-length blob as "no properties" (sqlite3_column_blob yields NULL).
    if blob.is_empty() {
        return Ok(Vec::new());
    }
    skel::parse_proplist(&Skel::parse(blob)?)
}

/// Encodes properties sorted by name, so equal sets always produce equal bytes.
pub(crate) fn encode_props(props: &Props) -> Vec<u8> {
    let mut sorted = props.clone();
    sorted.sort();
    skel::unparse_proplist(&sorted).unparse()
}

fn unsigned(v: Option<i64>) -> Option<u64> {
    v.and_then(|n| u64::try_from(n).ok())
}

/// Column list shared by every node query; order matches [`node_from_row`].
pub(crate) const NODE_COLUMNS: &str = "local_relpath, op_depth, presence, kind, repos_id, \
    repos_path, revision, depth, checksum, symlink_target, changed_revision, changed_date, \
    changed_author, translated_size, last_mod_time, properties, moved_here, moved_to, \
    file_external";

pub(crate) fn node_from_row(r: &Row<'_>) -> Result<Node> {
    let g = |e: rusqlite::Error| sql_err(e);
    let props: Option<Vec<u8>> = r.get(15).map_err(g)?;
    let moved_to: Option<String> = r.get(17).map_err(g)?;
    Ok(Node {
        relpath: rel(r.get(0).map_err(g)?)?,
        op_depth: u32::try_from(r.get::<_, i64>(1).map_err(g)?)
            .map_err(|_| corrupt("negative op_depth"))?,
        presence: Presence::parse(&r.get::<_, String>(2).map_err(g)?)?,
        kind: parse_kind(&r.get::<_, String>(3).map_err(g)?),
        repos_id: r.get(4).map_err(g)?,
        repos_relpath: r.get(5).map_err(g)?,
        revision: unsigned(r.get(6).map_err(g)?),
        depth: r
            .get::<_, Option<String>>(7)
            .map_err(g)?
            .map(|d| parse_depth(&d)),
        checksum: r.get(8).map_err(g)?,
        symlink_target: r.get(9).map_err(g)?,
        changed_rev: unsigned(r.get(10).map_err(g)?),
        changed_date: r.get(11).map_err(g)?,
        changed_author: r.get(12).map_err(g)?,
        translated_size: r.get(13).map_err(g)?,
        last_mod_time: r.get(14).map_err(g)?,
        properties: props.as_deref().map(decode_props).transpose()?,
        moved_here: r
            .get::<_, Option<i64>>(16)
            .map_err(g)?
            .is_some_and(|v| v != 0),
        moved_to: moved_to.map(rel).transpose()?,
        file_external: r
            .get::<_, Option<i64>>(18)
            .map_err(g)?
            .is_some_and(|v| v != 0),
    })
}

/// SQL boolean expression: `child` is a strict descendant of `parent` (both SQL operands).
/// Same range form as svn's `IS_STRICT_DESCENDANT_OF` macro (index friendly, case sensitive).
pub(crate) fn strict_descendant(child: &str, parent: &str) -> String {
    format!(
        "((({child}) > (CASE ({parent}) WHEN '' THEN '' ELSE ({parent}) || '/' END)) \
         AND (({child}) < CASE ({parent}) WHEN '' THEN X'FFFF' ELSE ({parent}) || '0' END))"
    )
}

/// Number of path segments of a relpath (`''` is 0), the `op_depth` of an operation rooted there.
pub(crate) fn relpath_depth(p: &RelPath) -> u32 {
    if p.is_root() {
        0
    } else {
        u32::try_from(p.components().count()).unwrap_or(u32::MAX)
    }
}

pub(crate) fn parent_text(p: &RelPath) -> Option<String> {
    p.parent().map(|x| x.as_str().to_owned())
}

/// Highest-`op_depth` row of `path` (the `NODES_CURRENT` row).
pub(crate) fn read_current(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Option<Node>> {
    let sql = format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
         ORDER BY op_depth DESC LIMIT 1"
    );
    query_one(conn, &sql, params![wc_id, path.as_str()])
}

/// BASE (`op_depth = 0`) row of `path`.
pub(crate) fn read_base(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Option<Node>> {
    read_layer(conn, wc_id, path, 0)
}

/// Row of `path` at exactly `op_depth`.
pub(crate) fn read_layer(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    op_depth: u32,
) -> Result<Option<Node>> {
    let sql = format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = ?3"
    );
    query_one(conn, &sql, params![wc_id, path.as_str(), op_depth])
}

fn query_one(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Option<Node>> {
    conn.query_row(sql, p, |r| Ok(node_from_row(r)))
        .optional()
        .map_err(sql_err)?
        .transpose()
}

/// Distinct child paths (any layer, any presence) of `parent`, sorted.
pub(crate) fn children(conn: &Connection, wc_id: i64, parent: &RelPath) -> Result<Vec<RelPath>> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT local_relpath FROM nodes WHERE wc_id = ?1 AND parent_relpath = ?2 \
             ORDER BY local_relpath",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![wc_id, parent.as_str()], |r| r.get::<_, String>(0))
        .map_err(sql_err)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(rel(row.map_err(sql_err)?)?);
    }
    Ok(out)
}

/// Every row (all layers) of the subtree of `root`, ordered by path then `op_depth`.
pub(crate) fn subtree_rows(conn: &Connection, wc_id: i64, root: &RelPath) -> Result<Vec<Node>> {
    let sql = format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE wc_id = ?1 AND (local_relpath = ?2 OR {}) \
         ORDER BY local_relpath, op_depth",
        strict_descendant("local_relpath", "?2")
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt.query(params![wc_id, root.as_str()]).map_err(sql_err)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        out.push(node_from_row(r)?);
    }
    Ok(out)
}
