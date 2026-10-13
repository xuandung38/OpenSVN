//! Bulk read of the NODES and ACTUAL_NODE rows below a status target.
//!
//! Status needs the state of every node of a subtree; one ordered query per table is far cheaper
//! than a `read_node` round trip per path (100k files). The per-path derivation mirrors what
//! `svn_wc__db_read_children_info` computes for `assemble_status`.

use std::collections::HashMap;

use rusqlite::{Connection, params};
use svn_types::{ErrorCode, NodeKind, RelPath, Result, SvnError};

use crate::conflicts::ConflictSkel;
use crate::db::sql_err;
use crate::pristine::Sha1Digest;

/// Presence of a NODES row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    Normal,
    Incomplete,
    NotPresent,
    BaseDeleted,
    Excluded,
    ServerExcluded,
}

impl Presence {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "normal" => Self::Normal,
            "incomplete" => Self::Incomplete,
            "not-present" => Self::NotPresent,
            "base-deleted" => Self::BaseDeleted,
            "excluded" => Self::Excluded,
            "server-excluded" => Self::ServerExcluded,
            other => {
                return Err(SvnError::new(ErrorCode::Corrupt, format!("wc.db: unknown presence {other:?}")));
            }
        })
    }
}

/// `svn_wc__db_status_t` of the current (highest layer) row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbStatus {
    Normal,
    Added,
    Deleted,
    Incomplete,
    NotPresent,
    Excluded,
}

/// Everything status needs to know about one versioned path.
#[derive(Debug, Clone)]
pub(crate) struct NodeRec {
    pub status: DbStatus,
    pub kind: NodeKind,
    pub op_depth: u32,
    pub have_base: bool,
    pub have_more_work: bool,
    pub copied: bool,
    pub op_root: bool,
    pub moved_here: bool,
    pub moved_to: Option<String>,
    /// Presence of the layer right below the top working layer.
    pub below_working: Option<Presence>,
    pub checksum: Option<Sha1Digest>,
    pub translated_size: Option<i64>,
    pub last_mod_time: Option<i64>,
    pub repos_relpath: Option<String>,
    /// Revision of the BASE layer.
    pub base_revision: Option<u64>,
    pub changed_rev: Option<u64>,
    pub changed_author: Option<String>,
    pub changed_date: Option<i64>,
    /// The (pristine) properties are not empty.
    pub had_props: bool,
    /// `svn:special` is among the pristine properties.
    pub special: bool,
    pub file_external: bool,
    /// BASE layer is a directory that is not complete.
    pub incomplete: bool,
}

/// ACTUAL_NODE state of one path.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActualRec {
    /// Local property changes are recorded.
    pub props_mod: bool,
    pub changelist: Option<String>,
    pub conflict: Option<ConflictSkel>,
}

fn node_kind(word: &str) -> NodeKind {
    match word {
        "file" => NodeKind::File,
        "dir" => NodeKind::Dir,
        "symlink" => NodeKind::Symlink,
        _ => NodeKind::None,
    }
}

/// Number of path components (`0` for the root); an `op_depth` equal to this marks an op root.
fn depth_of(relpath: &str) -> u32 {
    if relpath.is_empty() { 0 } else { u32::try_from(relpath.split('/').count()).unwrap_or(u32::MAX) }
}

/// SQL predicate selecting `target` and everything below it. Parameter `?2` is the target.
fn subtree(column: &str) -> String {
    format!("(?2 = '' OR {column} = ?2 OR ({column} >= ?2 || '/' AND {column} < ?2 || '0'))")
}

struct Row {
    relpath: String,
    op_depth: u32,
    presence: Presence,
    kind: NodeKind,
    repos_id: Option<i64>,
    checksum: Option<String>,
    translated_size: Option<i64>,
    last_mod_time: Option<i64>,
    repos_path: Option<String>,
    revision: Option<u64>,
    changed_rev: Option<u64>,
    changed_date: Option<i64>,
    changed_author: Option<String>,
    moved_here: bool,
    moved_to: Option<String>,
    file_external: bool,
    had_props: bool,
    special: bool,
}

fn build(group: &[Row]) -> Option<NodeRec> {
    let top = group.first()?;
    let have_base = group.iter().any(|r| r.op_depth == 0);
    let working = group.iter().filter(|r| r.op_depth > 0).count();
    let status = if top.op_depth == 0 {
        match top.presence {
            Presence::Normal => DbStatus::Normal,
            Presence::Incomplete => DbStatus::Incomplete,
            Presence::NotPresent => DbStatus::NotPresent,
            Presence::Excluded | Presence::ServerExcluded => DbStatus::Excluded,
            Presence::BaseDeleted => DbStatus::Deleted,
        }
    } else {
        match top.presence {
            Presence::Normal => DbStatus::Added,
            Presence::Incomplete => DbStatus::Incomplete,
            Presence::BaseDeleted => DbStatus::Deleted,
            Presence::NotPresent => DbStatus::NotPresent,
            Presence::Excluded | Presence::ServerExcluded => DbStatus::Excluded,
        }
    };
    let copied = match status {
        DbStatus::Added => top.repos_id.is_some(),
        DbStatus::Deleted => !have_base || working > 1 || top.repos_id.is_some(),
        _ => false,
    };
    let base = group.iter().find(|r| r.op_depth == 0);
    let below_working = if top.op_depth > 0 { group.get(1).map(|r| r.presence) } else { None };
    let moved_to = group.iter().find_map(|r| r.moved_to.clone());
    Some(NodeRec {
        status,
        kind: top.kind,
        op_depth: top.op_depth,
        have_base,
        have_more_work: working > 1,
        copied,
        op_root: top.op_depth == depth_of(&top.relpath),
        moved_here: top.moved_here,
        moved_to,
        below_working,
        checksum: top.checksum.as_deref().and_then(Sha1Digest::from_svn_string),
        translated_size: top.translated_size,
        last_mod_time: top.last_mod_time,
        repos_relpath: top.repos_path.clone(),
        base_revision: base.and_then(|b| b.revision),
        changed_rev: top.changed_rev,
        changed_author: top.changed_author.clone(),
        changed_date: top.changed_date,
        had_props: top.had_props,
        special: top.special,
        file_external: top.file_external,
        incomplete: base.is_some_and(|b| b.presence == Presence::Incomplete),
    })
}

/// Loads the current node state of `target` and all its descendants, keyed by relpath.
pub(crate) fn load_nodes(conn: &Connection, wc_id: i64, target: &RelPath) -> Result<HashMap<String, NodeRec>> {
    let sql = format!(
        "SELECT local_relpath, op_depth, presence, kind, repos_id, checksum, translated_size, last_mod_time, \
         repos_path, revision, changed_revision, changed_date, changed_author, moved_here, moved_to, \
         file_external, properties \
         FROM nodes WHERE wc_id = ?1 AND {} ORDER BY local_relpath, op_depth DESC",
        subtree("local_relpath")
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt.query(params![wc_id, target.as_str()]).map_err(sql_err)?;
    let mut out = HashMap::new();
    let mut group: Vec<Row> = Vec::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        let props: Option<Vec<u8>> = r.get(16).map_err(sql_err)?;
        let relpath: String = r.get(0).map_err(sql_err)?;
        let presence: String = r.get(2).map_err(sql_err)?;
        let kind: String = r.get(3).map_err(sql_err)?;
        let row = Row {
            op_depth: r.get::<_, i64>(1).map_err(sql_err)?.try_into().unwrap_or(0),
            presence: Presence::parse(&presence)?,
            kind: node_kind(&kind),
            repos_id: r.get(4).map_err(sql_err)?,
            checksum: r.get(5).map_err(sql_err)?,
            translated_size: r.get(6).map_err(sql_err)?,
            last_mod_time: r.get(7).map_err(sql_err)?,
            repos_path: r.get(8).map_err(sql_err)?,
            revision: r.get::<_, Option<i64>>(9).map_err(sql_err)?.and_then(|v| u64::try_from(v).ok()),
            changed_rev: r.get::<_, Option<i64>>(10).map_err(sql_err)?.and_then(|v| u64::try_from(v).ok()),
            changed_date: r.get(11).map_err(sql_err)?,
            changed_author: r.get(12).map_err(sql_err)?,
            moved_here: r.get::<_, Option<i64>>(13).map_err(sql_err)?.unwrap_or(0) != 0,
            moved_to: r.get(14).map_err(sql_err)?,
            file_external: r.get::<_, Option<i64>>(15).map_err(sql_err)?.is_some_and(|v| v != 0),
            had_props: props.as_deref().is_some_and(|p| !p.is_empty() && p != b"()"),
            special: props.as_deref().is_some_and(|p| p.windows(11).any(|w| w == b"svn:special")),
            relpath,
        };
        if group.first().is_some_and(|g| g.relpath != row.relpath) {
            if let Some(rec) = build(&group) {
                out.insert(group[0].relpath.clone(), rec);
            }
            group.clear();
        }
        group.push(row);
    }
    if let Some(rec) = build(&group) {
        out.insert(group[0].relpath.clone(), rec);
    }
    Ok(out)
}

/// Loads the ACTUAL_NODE rows of `target` and its descendants.
pub(crate) fn load_actual(conn: &Connection, wc_id: i64, target: &RelPath) -> Result<HashMap<String, ActualRec>> {
    let sql = format!(
        "SELECT local_relpath, properties IS NOT NULL, changelist, conflict_data FROM actual_node \
         WHERE wc_id = ?1 AND {}",
        subtree("local_relpath")
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt.query(params![wc_id, target.as_str()]).map_err(sql_err)?;
    let mut out = HashMap::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        let relpath: String = r.get(0).map_err(sql_err)?;
        let blob: Option<Vec<u8>> = r.get(3).map_err(sql_err)?;
        let conflict = match blob {
            Some(b) if !b.is_empty() => Some(ConflictSkel::decode(&b)?),
            _ => None,
        };
        out.insert(
            relpath,
            ActualRec {
                props_mod: r.get::<_, i64>(1).map_err(sql_err)? != 0,
                changelist: r.get(2).map_err(sql_err)?,
                conflict,
            },
        );
    }
    Ok(out)
}
