//! Statement-level writers that run inside ONE caller-owned wc.db transaction.
//!
//! `WcDb` offers one transaction per operation; an update must change BASE, the local property
//! overlay, the conflict record and the work queue together, otherwise a crash could leave a new
//! base next to a working file derived from the old one. These helpers write the same rows the
//! facade methods write (checked against svn 1.15 by the oracle tests) and are meant to be
//! replaced by a facade entry point once `svn-wc` offers one.

use rusqlite::{Transaction, params};
use svn_types::{ErrorCode, RelPath, Result, SvnError};
use svn_wc::db::{BaseNode, Props, sql_err};
use svn_wc::skel::{self, Skel};

fn parent_text(path: &RelPath) -> Option<String> {
    path.parent().map(|p| p.as_str().to_owned())
}

fn depth_word(d: svn_types::Depth) -> &'static str {
    use svn_types::Depth;
    match d {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Infinity => "infinity",
        Depth::Exclude => "exclude",
        Depth::Unknown => "unknown",
    }
}

fn kind_word(k: svn_types::NodeKind) -> &'static str {
    match k {
        svn_types::NodeKind::File => "file",
        svn_types::NodeKind::Dir => "dir",
        svn_types::NodeKind::Symlink => "symlink",
        _ => "unknown",
    }
}

fn encode_props(props: &Props) -> Vec<u8> {
    let mut sorted = props.clone();
    sorted.sort();
    skel::unparse_proplist(&sorted).unparse()
}

fn to_i64(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| SvnError::new(ErrorCode::Internal, "revision out of range"))
}

/// Appends serialized work items to the queue.
pub fn queue_work(tx: &Transaction<'_>, items: &[Skel]) -> Result<()> {
    for item in items {
        tx.execute("INSERT INTO work_queue (work) VALUES (?1)", [item.unparse()])
            .map_err(sql_err)?;
    }
    Ok(())
}

fn ensure_repository(tx: &Transaction<'_>, root: &str, uuid: &str) -> Result<i64> {
    use rusqlite::OptionalExtension;
    if let Some(id) = tx
        .query_row("SELECT id FROM repository WHERE root = ?1", [root], |r| r.get(0))
        .optional()
        .map_err(sql_err)?
    {
        return Ok(id);
    }
    tx.execute("INSERT INTO repository (root, uuid) VALUES (?1, ?2)", [root, uuid])
        .map_err(sql_err)?;
    Ok(tx.last_insert_rowid())
}

/// Inserts or replaces the BASE row of `path`.
pub fn base_add(tx: &Transaction<'_>, wc_id: i64, path: &RelPath, n: &BaseNode) -> Result<()> {
    let repos_id = ensure_repository(tx, &n.repos_root_url, &n.repos_uuid)?;
    tx.execute(
        "INSERT OR REPLACE INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
         repos_path, revision, presence, kind, depth, checksum, symlink_target, changed_revision, \
         changed_date, changed_author, translated_size, last_mod_time, properties, \
         inherited_props, dav_cache) \
         VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
         ?18, ?19)",
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
            n.inherited_props
                .as_ref()
                .map(|i| skel::unparse_iprops(i).unparse()),
            n.dav_cache,
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Turns the BASE row of a directory into an `incomplete` one at `rev`, so an interrupted update
/// is restartable. `repos_relpath` relocates it (switch).
pub fn mark_incomplete(
    tx: &Transaction<'_>,
    wc_id: i64,
    path: &RelPath,
    rev: u64,
    repos_relpath: Option<&str>,
) -> Result<()> {
    tx.execute(
        "UPDATE nodes SET presence = 'incomplete', revision = ?3, \
         repos_path = COALESCE(?4, repos_path) \
         WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
        params![wc_id, path.as_str(), to_i64(rev)?, repos_relpath],
    )
    .map_err(sql_err)?;
    Ok(())
}

const ACTUAL_EMPTY: &str = "properties IS NULL AND conflict_old IS NULL AND conflict_new IS NULL \
    AND conflict_working IS NULL AND prop_reject IS NULL AND changelist IS NULL \
    AND text_mod IS NULL AND tree_conflict_data IS NULL AND conflict_data IS NULL \
    AND older_checksum IS NULL AND left_checksum IS NULL AND right_checksum IS NULL";

fn ensure_actual(tx: &Transaction<'_>, wc_id: i64, path: &RelPath) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO actual_node (wc_id, local_relpath, parent_relpath) \
         VALUES (?1, ?2, ?3)",
        params![wc_id, path.as_str(), parent_text(path)],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn prune_actual(tx: &Transaction<'_>, wc_id: i64, path: &RelPath) -> Result<()> {
    tx.execute(
        &format!(
            "DELETE FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2 AND {ACTUAL_EMPTY}"
        ),
        params![wc_id, path.as_str()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Stores the working property overlay (`None` clears it).
pub fn set_actual_props(
    tx: &Transaction<'_>,
    wc_id: i64,
    path: &RelPath,
    props: Option<&Props>,
) -> Result<()> {
    match props {
        Some(p) => {
            ensure_actual(tx, wc_id, path)?;
            tx.execute(
                "UPDATE actual_node SET properties = ?3 WHERE wc_id = ?1 AND local_relpath = ?2",
                params![wc_id, path.as_str(), encode_props(p)],
            )
            .map_err(sql_err)?;
        }
        None => {
            tx.execute(
                "UPDATE actual_node SET properties = NULL WHERE wc_id = ?1 AND local_relpath = ?2",
                params![wc_id, path.as_str()],
            )
            .map_err(sql_err)?;
            prune_actual(tx, wc_id, path)?;
        }
    }
    Ok(())
}

/// The `conflict_data` skel stored for `path`.
pub fn read_conflict_skel(tx: &Transaction<'_>, wc_id: i64, path: &RelPath) -> Result<Option<Skel>> {
    use rusqlite::OptionalExtension;
    let blob: Option<Option<Vec<u8>>> = tx
        .query_row(
            "SELECT conflict_data FROM actual_node WHERE wc_id = ?1 AND local_relpath = ?2",
            params![wc_id, path.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    blob.flatten().map(|b| Skel::parse(&b)).transpose()
}

/// Stores the `conflict_data` skel of `path`.
pub fn write_conflict_skel(
    tx: &Transaction<'_>,
    wc_id: i64,
    path: &RelPath,
    conflict: &Skel,
) -> Result<()> {
    ensure_actual(tx, wc_id, path)?;
    tx.execute(
        "UPDATE actual_node SET conflict_data = ?3 WHERE wc_id = ?1 AND local_relpath = ?2",
        params![wc_id, path.as_str(), conflict.unparse()],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// `(local_relpath = ?2 OR strict descendant of ?2)` with svn's index friendly range form.
const SCOPE: &str = "(local_relpath = ?2 OR (local_relpath > (CASE ?2 WHEN '' THEN '' ELSE ?2 || '/' END) \
    AND local_relpath < CASE ?2 WHEN '' THEN X'FFFF' ELSE ?2 || '0' END))";

/// Rewrites the repository path prefix of the BASE subtree at `root` (switch).
pub fn relocate_subtree(
    tx: &Transaction<'_>,
    wc_id: i64,
    root: &RelPath,
    old_prefix: &str,
    new_prefix: &str,
) -> Result<()> {
    if old_prefix == new_prefix {
        return Ok(());
    }
    let sql = format!(
        "UPDATE nodes SET repos_path = CASE WHEN repos_path = ?3 THEN ?4 \
         ELSE ?4 || substr(repos_path, length(?3) + 1) END \
         WHERE wc_id = ?1 AND op_depth = 0 AND {SCOPE} AND repos_path IS NOT NULL \
         AND (repos_path = ?3 OR substr(repos_path, 1, length(?3) + 1) = ?3 || '/')"
    );
    tx.execute(&sql, params![wc_id, root.as_str(), old_prefix, new_prefix])
        .map_err(sql_err)?;
    Ok(())
}

/// Sets the BASE revision of the whole subtree at `root` to `rev` (`bump_revisions_post_update`),
/// except below `skipped` paths, which keep the revision they have.
pub fn bump_revisions(
    tx: &Transaction<'_>,
    wc_id: i64,
    root: &RelPath,
    rev: u64,
    skipped: &[RelPath],
) -> Result<()> {
    let mut kept: Vec<(String, i64)> = Vec::new();
    for s in skipped {
        let sql = format!(
            "SELECT local_relpath, revision FROM nodes WHERE wc_id = ?1 AND op_depth = 0 \
             AND revision IS NOT NULL AND {SCOPE}"
        );
        let mut stmt = tx.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params![wc_id, s.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sql_err)?;
        for row in rows {
            kept.push(row.map_err(sql_err)?);
        }
    }
    let sql = format!(
        "UPDATE nodes SET revision = ?3 WHERE wc_id = ?1 AND op_depth = 0 \
         AND presence IN ('normal', 'incomplete', 'not-present') \
         AND revision IS NOT NULL AND revision != ?3 AND {SCOPE}"
    );
    tx.execute(&sql, params![wc_id, root.as_str(), to_i64(rev)?])
        .map_err(sql_err)?;
    for (path, old) in kept {
        tx.execute(
            "UPDATE nodes SET revision = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0",
            params![wc_id, path, old],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}

/// Sets the recorded depth of a BASE directory.
pub fn set_depth(tx: &Transaction<'_>, wc_id: i64, path: &RelPath, depth: svn_types::Depth) -> Result<()> {
    tx.execute(
        "UPDATE nodes SET depth = ?3 WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = 0 \
         AND kind = 'dir'",
        params![wc_id, path.as_str(), depth_word(depth)],
    )
    .map_err(sql_err)?;
    Ok(())
}
