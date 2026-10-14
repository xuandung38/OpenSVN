//! Write side of the NODES model: BASE updates (checkout/update/commit) and working-layer
//! operations (add, delete, copy, move, revert).
//!
//! Every function expects to run inside one write transaction ([`super::WcDb::with_write`]) and
//! only touches the database: moving or deleting files on disk is the caller's job, expressed as
//! work items queued in the same transaction. Row shapes were checked against what svn 1.15.0
//! writes for the same operation (see `tests/nodes_ops_vs_svn.rs`).

use rusqlite::{Connection, OptionalExtension, params};
use svn_types::{Depth, ErrorCode, NodeKind, RelPath, Result, SvnError};

use super::actual;
use super::connection::sql_err;
use super::nodes::{
    self, Node, Presence, Props, depth_word, encode_props, kind_word, parent_text, read_base,
    read_current, relpath_depth, strict_descendant,
};
use crate::skel::{self, InheritedProps, Skel};

fn not_versioned(path: &RelPath) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("'{path}' is not under version control"),
    )
}

/// A working copy shape this version deliberately does not handle yet. It is a user-facing
/// condition (not an engine bug), so it carries [`ErrorCode::WcFormatUnsupported`].
fn unsupported(msg: String) -> SvnError {
    SvnError::new(
        ErrorCode::WcFormatUnsupported,
        format!("not supported yet: {msg}"),
    )
}

fn require_current(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Node> {
    read_current(conn, wc_id, path)?.ok_or_else(|| not_versioned(path))
}

/// Parent of `path` must exist and be a live (normal/incomplete) directory.
fn require_live_parent(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Node> {
    let parent = path
        .parent()
        .ok_or_else(|| SvnError::new(ErrorCode::Internal, "the working copy root has no parent"))?;
    let node = require_current(conn, wc_id, &parent)?;
    if !node.presence.is_present() {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!(
                "parent '{parent}' of '{path}' is {}",
                node.presence.as_str()
            ),
        ));
    }
    Ok(node)
}

/// Appends serialized work items to the queue in the caller's transaction.
pub(crate) fn queue_work(conn: &Connection, items: &[Skel]) -> Result<()> {
    for item in items {
        conn.execute(
            "INSERT INTO work_queue (work) VALUES (?1)",
            [item.unparse()],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}

/// Returns `REPOSITORY.id` for `(root, uuid)`, inserting the row when missing.
pub(crate) fn ensure_repository(conn: &Connection, root: &str, uuid: &str) -> Result<i64> {
    let found: Option<i64> = conn
        .query_row("SELECT id FROM repository WHERE root = ?1", [root], |r| {
            r.get(0)
        })
        .optional()
        .map_err(sql_err)?;
    if let Some(id) = found {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO repository (root, uuid) VALUES (?1, ?2)",
        [root, uuid],
    )
    .map_err(sql_err)?;
    Ok(conn.last_insert_rowid())
}

// ---------------------------------------------------------------------------------------------
// BASE layer
// ---------------------------------------------------------------------------------------------

/// A BASE node as written by checkout/update.
#[derive(Debug, Clone)]
pub struct BaseNode {
    /// Kind of the node.
    pub kind: NodeKind,
    /// `Normal`, `Incomplete`, `NotPresent`, `Excluded` or `ServerExcluded`.
    pub presence: Presence,
    /// Repository root URL.
    pub repos_root_url: String,
    /// Repository UUID.
    pub repos_uuid: String,
    /// Path inside the repository (no leading slash).
    pub repos_relpath: String,
    /// BASE revision.
    pub revision: u64,
    /// Last changed revision.
    pub changed_rev: Option<u64>,
    /// Last changed date (APR microseconds).
    pub changed_date: Option<i64>,
    /// Last changed author.
    pub changed_author: Option<String>,
    /// Directory depth.
    pub depth: Option<Depth>,
    /// Pristine properties.
    pub properties: Option<Props>,
    /// Pristine SHA-1 (`$sha1$hex`); the PRISTINE row must already exist for refcounting.
    pub checksum: Option<String>,
    /// Symlink target.
    pub symlink_target: Option<String>,
    /// Working file size known to match the pristine (cache).
    pub translated_size: Option<i64>,
    /// Working file mtime known to match the pristine (cache).
    pub last_mod_time: Option<i64>,
    /// Inherited properties (kept on update/switch roots).
    pub inherited_props: Option<InheritedProps>,
    /// Opaque DAV cache blob.
    pub dav_cache: Option<Vec<u8>>,
}

pub(crate) fn base_add(conn: &Connection, wc_id: i64, path: &RelPath, n: &BaseNode) -> Result<()> {
    let repos_id = ensure_repository(conn, &n.repos_root_url, &n.repos_uuid)?;
    let to_i64 = |v: u64| {
        i64::try_from(v).map_err(|_| SvnError::new(ErrorCode::Internal, "revision out of range"))
    };
    conn.execute(
        "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
         repos_path, revision, presence, kind, depth, checksum, symlink_target, changed_revision, \
         changed_date, changed_author, translated_size, last_mod_time, properties, \
         inherited_props, dav_cache) \
         VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            wc_id,
            path.as_str(),
            parent_text(path),
            repos_id,
            n.repos_relpath,
            to_i64(n.revision)?,
            n.presence.as_str(),
            kind_word(n.kind),
            n.depth.map(depth_word),
            n.checksum,
            n.symlink_target,
            n.changed_rev.map(to_i64).transpose()?,
            n.changed_date,
            n.changed_author,
            n.translated_size,
            n.last_mod_time,
            n.properties.as_ref().map(encode_props),
            n.inherited_props.as_ref().map(|i| skel::unparse_iprops(i).unparse()),
            n.dav_cache,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Removes the BASE node of `path` and its BASE descendants. Working layers that only recorded
/// a delete of that BASE become meaningless and go too; ACTUAL leftovers without conflicts are
/// dropped.
pub(crate) fn base_remove(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<()> {
    let scope = scope_sql();
    detach_moves(conn, wc_id, path, 0)?;
    conn.execute(
        &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth = 0"),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    conn.execute(
        &format!(
            "DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND presence = 'base-deleted' \
             AND NOT EXISTS (SELECT 1 FROM nodes l WHERE l.wc_id = nodes.wc_id \
                AND l.local_relpath = nodes.local_relpath AND l.op_depth < nodes.op_depth)"
        ),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    actual::drop_orphans(conn, wc_id, path)
}

pub(crate) fn set_depth(conn: &Connection, wc_id: i64, path: &RelPath, depth: Depth) -> Result<()> {
    if !matches!(
        depth,
        Depth::Empty | Depth::Files | Depth::Immediates | Depth::Infinity
    ) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "set_depth needs a concrete depth",
        ));
    }
    let n = conn
        .execute(
            "UPDATE nodes SET depth = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0 \
             AND kind = 'dir'",
            params![wc_id, path.as_str(), depth_word(depth)],
        )
        .map_err(sql_err)?;
    if n == 0 {
        return Err(not_versioned(path));
    }
    Ok(())
}

/// Records the stat cache of the current (highest layer) file node.
pub(crate) fn record_fileinfo(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    translated_size: Option<i64>,
    last_mod_time: Option<i64>,
) -> Result<()> {
    let n = conn
        .execute(
            "UPDATE nodes SET translated_size = ?3, last_mod_time = ?4 \
             WHERE wc_id = ?1 AND local_relpath = ?2 \
             AND op_depth = (SELECT MAX(op_depth) FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2)",
            params![wc_id, path.as_str(), translated_size, last_mod_time],
        )
        .map_err(sql_err)?;
    if n == 0 {
        return Err(not_versioned(path));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Working layers
// ---------------------------------------------------------------------------------------------

/// `(local_relpath = ?2 OR strict descendant of ?2)`: the node and its subtree.
pub(crate) fn scope_sql() -> String {
    format!(
        "(local_relpath = ?2 OR {})",
        strict_descendant("local_relpath", "?2")
    )
}

/// Keeps `moved_to` / `moved_here` pairs consistent before rows at `op_depth >= min_depth` of
/// the subtree at `path` are removed.
pub(crate) fn detach_moves(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    min_depth: u32,
) -> Result<()> {
    let scope = scope_sql();
    // Source side loses its destination: the delete layer vanishes, so clear nothing here;
    // destination side being removed: forget the source's moved_to pointing at it.
    conn.execute(
        &format!(
            "UPDATE nodes SET moved_to = NULL WHERE wc_id = ?1 AND moved_to IN \
             (SELECT local_relpath FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth >= ?3 \
              AND moved_here = 1)"
        ),
        params![wc_id, path.as_str(), min_depth],
    )
    .map_err(sql_err)?;
    // Removing a delete layer that recorded a move: the destination is no longer "moved here".
    let mut stmt = conn
        .prepare(&format!(
            "SELECT moved_to FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth >= ?3 \
             AND moved_to IS NOT NULL"
        ))
        .map_err(sql_err)?;
    let dests: Vec<String> = stmt
        .query_map(params![wc_id, path.as_str(), min_depth], |r| r.get(0))
        .map_err(sql_err)?
        .collect::<std::result::Result<_, _>>()
        .map_err(sql_err)?;
    for dest in dests {
        let dest = nodes::rel(dest)?;
        conn.execute(
            &format!(
                "UPDATE nodes SET moved_here = NULL WHERE wc_id = ?1 AND op_depth = ?3 \
                 AND (local_relpath = ?2 OR {})",
                strict_descendant("local_relpath", "?2")
            ),
            params![wc_id, dest.as_str(), relpath_depth(&dest)],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}

pub(crate) fn op_add(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    kind: NodeKind,
    symlink_target: Option<&str>,
) -> Result<()> {
    if !matches!(kind, NodeKind::File | NodeKind::Dir | NodeKind::Symlink) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "op_add needs a file, dir or symlink kind",
        ));
    }
    require_live_parent(conn, wc_id, path)?;
    if let Some(cur) = read_current(conn, wc_id, path)?
        && !matches!(cur.presence, Presence::BaseDeleted | Presence::NotPresent)
    {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!("'{path}' is already under version control"),
        ));
    }
    conn.execute(
        "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, presence, \
         kind, depth, symlink_target, changed_date) VALUES (?1, ?2, ?3, ?4, 'normal', ?5, ?6, ?7, 0)",
        params![
            wc_id,
            path.as_str(),
            relpath_depth(path),
            parent_text(path),
            kind_word(kind),
            (kind == NodeKind::Dir).then_some("infinity"),
            symlink_target,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

pub(crate) fn op_delete(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<()> {
    let cur = require_current(conn, wc_id, path)?;
    if path.is_root() {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "cannot delete the working copy root",
        ));
    }
    if !cur.presence.is_present() {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!(
                "'{path}' is {} and cannot be deleted",
                cur.presence.as_str()
            ),
        ));
    }
    let d = relpath_depth(path);
    let scope = scope_sql();
    detach_moves(conn, wc_id, path, d)?;
    // Layers at or above the delete are subsumed by it (added files below, earlier deletes ...).
    conn.execute(
        &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth >= ?3"),
        params![wc_id, path.as_str(), d],
    )
    .map_err(sql_err)?;
    // Whatever is still visible below becomes a base-deleted layer rooted here.
    conn.execute(
        &format!(
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, presence, kind) \
             SELECT wc_id, local_relpath, ?3, parent_relpath, 'base-deleted', kind FROM nodes AS n \
             WHERE wc_id = ?1 AND {scope} AND presence IN ('normal', 'incomplete') \
             AND op_depth = (SELECT MAX(op_depth) FROM nodes AS n2 WHERE n2.wc_id = n.wc_id \
                             AND n2.local_relpath = n.local_relpath AND n2.op_depth < ?3)"
        ),
        params![wc_id, path.as_str(), d],
    )
    .map_err(sql_err)?;
    // Local property changes and conflicts die with the node; changelist membership stays.
    actual::clear_modifications(conn, wc_id, path, true)?;
    actual::drop_orphans(conn, wc_id, path)
}

/// Copies the subtree at `src` to `dst` as a working layer rooted at `dst`, the way svn does:
/// the copy takes the BASE-like row of every path (the highest row at or below the source's own
/// operation depth) and re-creates the delete layers that the source subtree carries, re-based to
/// the destination depth (so a locally deleted child is deleted in the copy too).
///
/// Mixed-revision sources, not-present and excluded children are copied like svn does (see the
/// layering comment in the body). Not supported yet (explicit [`ErrorCode::WcFormatUnsupported`]):
/// locally added/copied nodes inside the source and incomplete children.
pub(crate) fn op_copy(
    conn: &Connection,
    wc_id: i64,
    src: &RelPath,
    dst: &RelPath,
    moved_here: bool,
) -> Result<()> {
    if dst.starts_with(src) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "cannot copy a node into itself",
        ));
    }
    require_live_parent(conn, wc_id, dst)?;
    if let Some(cur) = read_current(conn, wc_id, dst)?
        && !matches!(cur.presence, Presence::BaseDeleted | Presence::NotPresent)
    {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!("'{dst}' is already under version control"),
        ));
    }
    let root = require_current(conn, wc_id, src)?;
    if !root.presence.is_present() {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{src}' is not copyable ({})", root.presence.as_str()),
        ));
    }
    let src_depth = relpath_depth(src);
    let dst_depth = relpath_depth(dst);
    // Plan first, write afterwards: an unsupported shape must not leave a partial copy.
    struct Planned {
        target: RelPath,
        base: Node,
        carried_deletes: Vec<Node>,
    }
    let mut plan: Vec<Planned> = Vec::new();
    let mut group: Vec<Node> = Vec::new();
    let all = nodes::subtree_rows(conn, wc_id, src)?;
    let flush = |group: &mut Vec<Node>, plan: &mut Vec<Planned>| -> Result<()> {
        if group.is_empty() {
            return Ok(());
        }
        let path = group[0].relpath.clone();
        let rows = std::mem::take(group);
        let Some(base) = rows.iter().rev().find(|r| r.op_depth <= src_depth).cloned() else {
            return Err(unsupported(format!("copying locally added node '{path}'")));
        };
        match base.presence {
            Presence::Normal
            | Presence::NotPresent
            | Presence::Excluded
            | Presence::ServerExcluded => {}
            Presence::BaseDeleted => return Ok(()), // deleted together with the source itself
            other => {
                return Err(unsupported(format!(
                    "copying '{path}' which is {} in the source",
                    other.as_str()
                )));
            }
        }
        let mut carried_deletes = Vec::new();
        for r in rows.iter().filter(|r| r.op_depth > src_depth) {
            if r.presence != Presence::BaseDeleted {
                return Err(unsupported(format!("copying locally added node '{path}'")));
            }
            carried_deletes.push(r.clone());
        }
        let suffix = path
            .as_str()
            .strip_prefix(src.as_str())
            .unwrap_or("")
            .trim_start_matches('/');
        let target = if suffix.is_empty() {
            dst.clone()
        } else {
            RelPath::new(format!("{}/{}", dst.as_str(), suffix).trim_start_matches('/'))?
        };
        plan.push(Planned {
            target,
            base,
            carried_deletes,
        });
        Ok(())
    };
    for row in all {
        if group.first().is_some_and(|g| g.relpath != row.relpath) {
            flush(&mut group, &mut plan)?;
        }
        group.push(row);
    }
    flush(&mut group, &mut plan)?;
    for p in &plan {
        if p.base.repos_id.is_none() || p.base.repos_relpath.is_none() || p.base.revision.is_none()
        {
            return Err(unsupported(format!(
                "copying locally added node '{}'",
                p.base.relpath
            )));
        }
    }
    // Mixed revisions, like svn: a live node whose repository location differs from where its
    // parent's location says it should be starts a layer of its own at its own depth, and the
    // parent's layer gets a not-present placeholder for it. Its descendants join that layer.
    // Excluded and not-present children keep their presence inside the parent's layer.
    let mut layers: std::collections::HashMap<String, (u32, Node)> =
        std::collections::HashMap::new();
    for p in plan {
        let node = &p.base;
        let parent_entry = layers.get(parent_text(&p.target).as_deref().unwrap_or(""));
        let mut layer = parent_entry.map_or(dst_depth, |(l, _)| *l);
        if node.presence == Presence::Normal
            && let Some((parent_layer, parent)) = parent_entry
        {
            let name = p.target.file_name().unwrap_or_default();
            let parent_path = parent.repos_relpath.as_deref().unwrap_or("");
            let expected = if parent_path.is_empty() {
                name.to_owned()
            } else {
                format!("{parent_path}/{name}")
            };
            if node.revision != parent.revision
                || node.repos_id != parent.repos_id
                || node.repos_relpath.as_deref() != Some(expected.as_str())
            {
                conn.execute(
                    "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                     repos_id, repos_path, revision, presence, kind) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'not-present', ?8)",
                    params![
                        wc_id,
                        p.target.as_str(),
                        parent_layer,
                        parent_text(&p.target),
                        node.repos_id,
                        node.repos_relpath,
                        node.revision.and_then(|v| i64::try_from(v).ok()),
                        kind_word(node.kind),
                    ],
                )
                .map_err(sql_err)?;
                layer = relpath_depth(&p.target);
            }
        }
        if node.presence == Presence::Normal {
            layers.insert(p.target.as_str().to_owned(), (layer, node.clone()));
        }
        if node.presence == Presence::NotPresent {
            conn.execute(
                "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, \
                 repos_id, repos_path, revision, presence, kind, changed_date) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'not-present', ?8, 0)",
                params![
                    wc_id,
                    p.target.as_str(),
                    layer,
                    parent_text(&p.target),
                    node.repos_id,
                    node.repos_relpath,
                    node.revision.and_then(|v| i64::try_from(v).ok()),
                    kind_word(node.kind),
                ],
            )
            .map_err(sql_err)?;
            continue;
        }
        conn.execute(
            "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
             repos_path, revision, presence, depth, kind, changed_revision, changed_date, \
             changed_author, checksum, properties, symlink_target, moved_here, translated_size, \
             last_mod_time) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?17, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
             ?18, ?19)",
            params![
                wc_id,
                p.target.as_str(),
                layer,
                parent_text(&p.target),
                node.repos_id,
                node.repos_relpath,
                node.revision.and_then(|v| i64::try_from(v).ok()),
                node.depth.map(depth_word),
                kind_word(node.kind),
                node.changed_rev.and_then(|v| i64::try_from(v).ok()),
                node.changed_date,
                node.changed_author,
                node.checksum,
                node.properties.as_ref().map(encode_props),
                node.symlink_target,
                moved_here.then_some(1),
                node.presence.as_str(),
                // The stat cache travels with the row (STMT_INSERT_WORKING_NODE_COPY_FROM).
                node.translated_size,
                node.last_mod_time,
            ],
        )
        .map_err(sql_err)?;
        for del in &p.carried_deletes {
            let shifted = del.op_depth - src_depth + dst_depth;
            conn.execute(
                "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, presence, kind, changed_date) \
                 VALUES (?1, ?2, ?3, ?4, 'base-deleted', ?5, 0)",
                params![wc_id, p.target.as_str(), shifted, parent_text(&p.target), kind_word(del.kind)],
            )
            .map_err(sql_err)?;
        }
        // Local property edits of the source travel as local edits of the copy.
        if let Some(local) = actual::read_props(conn, wc_id, &node.relpath)?
            && p.carried_deletes.is_empty()
        {
            actual::write_props(conn, wc_id, &p.target, Some(&local))?;
        }
    }
    Ok(())
}

/// `svn move` bookkeeping: copy, delete the source and point its delete layer at the
/// destination. Only a source that has something below its working layer (a BASE node or a lower
/// layer) leaves a delete layer behind, so only then is the destination "moved here"; moving a
/// plain local copy is a copy of it plus removal, like svn.
pub(crate) fn op_move(conn: &Connection, wc_id: i64, src: &RelPath, dst: &RelPath) -> Result<()> {
    let d = relpath_depth(src);
    let has_lower: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2 \
             AND op_depth < ?3 AND presence IN ('normal', 'incomplete'))",
            params![wc_id, src.as_str(), d],
            |r| r.get(0),
        )
        .map_err(sql_err)?;
    op_copy(conn, wc_id, src, dst, has_lower)?;
    op_delete(conn, wc_id, src)?;
    conn.execute(
        "UPDATE nodes SET moved_to = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = ?4 \
         AND presence = 'base-deleted'",
        params![wc_id, src.as_str(), dst.as_str(), d],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Reverts local changes at `path`, like `svn_wc__db_op_revert`.
///
/// * `recursive`: every working layer inside the subtree is removed (adds, deletes, copies on
///   descendants too) and local property/conflict state is cleared; changelists stay.
/// * not recursive: only the node's own layer (when `path` is an operation root, children of that
///   same operation included) or its own property/conflict state. An operation root that still
///   has separate operations below it is refused with svn's E155038 instead of reverting them
///   silently.
pub(crate) fn op_revert(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    recursive: bool,
) -> Result<()> {
    let cur = require_current(conn, wc_id, path)?;
    let d = relpath_depth(path);
    let scope = scope_sql();
    let sub = strict_descendant("local_relpath", "?2");
    if cur.op_depth > 0 {
        if cur.op_depth != d {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("'{path}' is part of a larger change; revert its root instead"),
            ));
        }
        if !recursive {
            let nested: bool = conn
                .query_row(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM nodes WHERE wc_id = ?1 AND {sub} AND op_depth > ?3)"
                    ),
                    params![wc_id, path.as_str(), d],
                    |r| r.get(0),
                )
                .map_err(sql_err)?;
            if nested {
                return Err(SvnError::new(
                    ErrorCode::WcInvalidOperation,
                    format!("can't revert '{path}' without reverting children"),
                )
                .with_svn_code(155_038));
            }
        }
        detach_moves(conn, wc_id, path, d)?;
        conn.execute(
            &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth >= ?3"),
            params![wc_id, path.as_str(), d],
        )
        .map_err(sql_err)?;
        actual::clear_modifications(conn, wc_id, path, true)?;
        return actual::drop_orphans(conn, wc_id, path);
    }
    if recursive {
        // The node itself is unchanged BASE; every working layer below it belongs to an
        // operation rooted inside the subtree.
        detach_moves(conn, wc_id, path, 1)?;
        conn.execute(
            &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth > 0"),
            params![wc_id, path.as_str()],
        )
        .map_err(sql_err)?;
        actual::drop_orphans(conn, wc_id, path)?;
    }
    actual::clear_modifications(conn, wc_id, path, recursive)
}

// ---------------------------------------------------------------------------------------------
// Properties, changelists, conflicts
// ---------------------------------------------------------------------------------------------

/// Pristine properties of the current node (empty when the layer carries none).
pub(crate) fn pristine_props(conn: &Connection, wc_id: i64, path: &RelPath) -> Result<Props> {
    let cur = require_current(conn, wc_id, path)?;
    Ok(if cur.presence.is_present() {
        cur.properties.unwrap_or_default()
    } else {
        Vec::new()
    })
}

fn sorted(p: &Props) -> Props {
    let mut s = p.clone();
    s.sort();
    s
}

/// Sets the working properties; a set equal to the pristine one clears the local change.
pub(crate) fn set_props(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    props: &Props,
) -> Result<()> {
    let pristine = pristine_props(conn, wc_id, path)?;
    if sorted(props) == sorted(&pristine) {
        actual::write_props(conn, wc_id, path, None)
    } else {
        actual::write_props(conn, wc_id, path, Some(props))
    }
}

pub(crate) fn set_changelist(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    name: Option<&str>,
) -> Result<()> {
    let cur = require_current(conn, wc_id, path)?;
    if name.is_some() && !cur.presence.is_present() {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("'{path}' cannot join a changelist"),
        ));
    }
    actual::write_changelist(conn, wc_id, path, name)
}

// ---------------------------------------------------------------------------------------------
// Commit post-processing
// ---------------------------------------------------------------------------------------------

/// What a successful commit tells the working copy about one committed path.
#[derive(Debug, Clone, Default)]
pub struct CommitInfo {
    /// New revision.
    pub new_rev: u64,
    /// Commit date (APR microseconds).
    pub date: Option<i64>,
    /// Commit author.
    pub author: Option<String>,
    /// New pristine checksum when the text changed (PRISTINE row must exist).
    pub checksum: Option<String>,
    /// New BASE properties; `None` uses the working properties (ACTUAL, else current).
    pub props: Option<Props>,
    /// File stat cache to store with the new BASE.
    pub translated_size: Option<i64>,
    /// File mtime cache to store with the new BASE.
    pub last_mod_time: Option<i64>,
    /// Keep changelist membership (svn drops it by default after commit).
    pub keep_changelist: bool,
}

fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| SvnError::new(ErrorCode::Internal, "revision out of range"))
}

/// Folds a committed path into BASE. Parents must be processed before children. A committed
/// delete leaves a `not-present` BASE row at the new revision (like svn) and removes the subtree.
///
/// Idempotent for children the commit of a replaced/copied root already converted: svn gathers a
/// commit item for every changed node, including `e/f2` deleted inside the copy `e`, but committing
/// `e` turned `e/f2` into a `not-present` BASE row at the new revision, so committing it again is a
/// no-op.
pub(crate) fn commit_node(
    conn: &Connection,
    wc_id: i64,
    path: &RelPath,
    info: &CommitInfo,
) -> Result<()> {
    let cur = require_current(conn, wc_id, path)?;
    let d = relpath_depth(path);
    let new_rev = to_i64(info.new_rev)?;
    if cur.op_depth == 0
        && cur.presence == Presence::NotPresent
        && cur.revision == Some(info.new_rev)
    {
        return Ok(());
    }
    if cur.presence == Presence::BaseDeleted {
        if cur.op_depth != d {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("'{path}' is deleted as part of a parent; commit the parent"),
            ));
        }
        let base = read_base(conn, wc_id, path)?;
        let scope = scope_sql();
        detach_moves(conn, wc_id, path, 0)?;
        conn.execute(
            &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope}"),
            params![wc_id, path.as_str()],
        )
        .map_err(sql_err)?;
        if let Some(b) = base {
            conn.execute(
                "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
                 repos_path, revision, presence, kind, changed_date) \
                 VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, 'not-present', ?7, 0)",
                params![
                    wc_id,
                    path.as_str(),
                    parent_text(path),
                    b.repos_id,
                    b.repos_relpath,
                    new_rev,
                    kind_word(b.kind)
                ],
            )
            .map_err(sql_err)?;
        }
        actual::drop_orphans(conn, wc_id, path)?;
        return Ok(());
    }
    if cur.presence != Presence::Normal {
        return Err(SvnError::new(
            ErrorCode::Internal,
            format!(
                "'{path}' is {} and cannot be committed",
                cur.presence.as_str()
            ),
        ));
    }
    let working_props = actual::read_props(conn, wc_id, path)?;
    let props = info
        .props
        .clone()
        .or(working_props)
        .or_else(|| cur.properties.clone());
    let checksum = info.checksum.clone().or_else(|| cur.checksum.clone());
    let (author, date) = (info.author.clone(), info.date);
    if cur.op_depth == 0 {
        conn.execute(
            "UPDATE nodes SET revision = ?3, changed_revision = ?3, changed_date = ?4, \
             changed_author = ?5, checksum = ?6, properties = ?7, translated_size = ?8, \
             last_mod_time = ?9 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
            params![
                wc_id,
                path.as_str(),
                new_rev,
                date,
                author,
                checksum,
                props.as_ref().map(encode_props),
                info.translated_size,
                info.last_mod_time
            ],
        )
        .map_err(sql_err)?;
    } else {
        if cur.op_depth != d {
            return Err(SvnError::new(
                ErrorCode::Internal,
                format!("'{path}' is a child of an uncommitted copy; commit the copy root"),
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "root cannot be added"))?;
        let pbase = read_base(conn, wc_id, &parent)?
            .filter(|p| p.presence == Presence::Normal)
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::Internal,
                    format!("parent of '{path}' is not committed yet"),
                )
            })?;
        let name = path.file_name().unwrap_or_default();
        let proot = pbase.repos_relpath.clone().unwrap_or_default();
        let new_root = if proot.is_empty() {
            name.to_owned()
        } else {
            format!("{proot}/{name}")
        };
        // Committing a replacement: the old BASE subtree and the delete layers that recorded its
        // removal are gone in the repository; separate pending adds below (higher layers) stay
        // for their own commit.
        if read_base(conn, wc_id, path)?.is_some() {
            detach_moves(conn, wc_id, path, 0)?;
            conn.execute(
                &format!(
                    "DELETE FROM nodes WHERE wc_id = ?1 AND {} AND op_depth = 0",
                    scope_sql()
                ),
                params![wc_id, path.as_str()],
            )
            .map_err(sql_err)?;
            conn.execute(
                &format!(
                    "DELETE FROM nodes WHERE wc_id = ?1 AND {} AND presence = 'base-deleted' AND op_depth = ?3",
                    scope_sql()
                ),
                params![wc_id, path.as_str(), cur.op_depth],
            )
            .map_err(sql_err)?;
        }
        // Root of the layer first, then the rest of the same layer.
        let layer: Vec<Node> = layer_nodes(conn, wc_id, path, cur.op_depth)?;
        for node in &layer {
            let is_root = node.relpath == *path;
            let suffix = node
                .relpath
                .as_str()
                .strip_prefix(path.as_str())
                .unwrap_or("")
                .trim_start_matches('/');
            let repos_path = if suffix.is_empty() {
                new_root.clone()
            } else {
                format!("{new_root}/{suffix}")
            };
            let (props, checksum, crev, cdate, cauthor) = if is_root {
                (
                    props.clone(),
                    checksum.clone(),
                    Some(new_rev),
                    date,
                    author.clone(),
                )
            } else {
                let p = match actual::read_props(conn, wc_id, &node.relpath)? {
                    Some(p) => Some(p),
                    None => node.properties.clone(),
                };
                (
                    p,
                    node.checksum.clone(),
                    node.changed_rev.and_then(|v| i64::try_from(v).ok()),
                    node.changed_date,
                    node.changed_author.clone(),
                )
            };
            conn.execute(
                "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
                 repos_path, revision, presence, kind, depth, checksum, symlink_target, changed_revision, \
                 changed_date, changed_author, translated_size, last_mod_time, properties) \
                 VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, 'normal', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    wc_id, node.relpath.as_str(), parent_text(&node.relpath), pbase.repos_id, repos_path, new_rev,
                    kind_word(node.kind), node.depth.map(depth_word), checksum, node.symlink_target, crev, cdate, cauthor,
                    if is_root { info.translated_size } else { None }, if is_root { info.last_mod_time } else { None },
                    props.as_ref().map(encode_props),
                ],
            )
            .map_err(sql_err)?;
        }
        let scope = scope_sql();
        // Nodes deleted inside the committed layer (delete layers above it) become not-present in
        // BASE, with their descendants gone, and the delete layers are consumed.
        let deleted: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT local_relpath, kind FROM nodes WHERE wc_id = ?1 AND {scope} \
                     AND presence = 'base-deleted' AND op_depth > ?3 AND local_relpath <> ?2"
                ))
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![wc_id, path.as_str(), cur.op_depth], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(sql_err)?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(sql_err)?
        };
        for (rel, kind) in &deleted {
            let suffix = rel
                .strip_prefix(path.as_str())
                .unwrap_or("")
                .trim_start_matches('/');
            conn.execute(
                "DELETE FROM nodes WHERE wc_id = ?1 AND op_depth = 0 \
                 AND (local_relpath = ?2 OR local_relpath LIKE ?2 || '/%' ESCAPE '\\')",
                params![
                    wc_id,
                    rel.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                ],
            )
            .map_err(sql_err)?;
            conn.execute(
                "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, repos_path, \
                 revision, presence, kind, changed_date) VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, 'not-present', ?7, 0)",
                params![
                    wc_id, rel, parent_text(&RelPath::new(rel.as_str())?), pbase.repos_id,
                    format!("{new_root}/{suffix}"), new_rev, kind,
                ],
            )
            .map_err(sql_err)?;
        }
        conn.execute(
            &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND presence = 'base-deleted' AND op_depth > ?3"),
            params![wc_id, path.as_str(), cur.op_depth],
        )
        .map_err(sql_err)?;
        detach_moves(conn, wc_id, path, cur.op_depth)?;
        conn.execute(
            &format!("DELETE FROM nodes WHERE wc_id = ?1 AND {scope} AND op_depth = ?3 AND presence IN ('normal', 'incomplete')"),
            params![wc_id, path.as_str(), cur.op_depth],
        )
        .map_err(sql_err)?;
    }
    actual::write_props(conn, wc_id, path, None)?;
    if !info.keep_changelist {
        actual::write_changelist(conn, wc_id, path, None)?;
    }
    Ok(())
}

/// Rows at exactly `op_depth` in the subtree of `path` that are live nodes, root first.
fn layer_nodes(conn: &Connection, wc_id: i64, path: &RelPath, op_depth: u32) -> Result<Vec<Node>> {
    let sql = format!(
        "SELECT {} FROM nodes WHERE wc_id = ?1 AND {} AND op_depth = ?3 AND presence = 'normal' \
         ORDER BY local_relpath",
        nodes::NODE_COLUMNS,
        scope_sql()
    );
    let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
    let mut rows = stmt
        .query(params![wc_id, path.as_str(), op_depth])
        .map_err(sql_err)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql_err)? {
        out.push(nodes::node_from_row(r)?);
    }
    Ok(out)
}
