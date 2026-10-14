//! Checks applied to a working copy after a crash and after recovery.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use svn_types::{NodeStatus, PropStatus, WcStatus};

use crate::{Result, err};

/// Opens `wc.db` read/write (never creating it): after a kill SQLite may have to roll back a hot
/// journal first, which a read-only connection cannot do. Nothing is written by the checks.
fn open_ro(wc: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        wc.join(".svn/wc.db"),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| err(format!("open wc.db: {e}")))
}

/// Number of WC_LOCK rows.
pub fn lock_rows(wc: &Path) -> Result<i64> {
    open_ro(wc)?
        .query_row("SELECT count(*) FROM wc_lock", [], |r| r.get(0))
        .map_err(|e| err(format!("wc_lock: {e}")))
}

/// `wc.db` passes SQLite's `integrity_check` and `foreign_key_check`, and its NODES rows are
/// consistent: every non-root node has a parent row and every BASE file row that is present
/// names a checksum whose pristine text exists in `.svn/pristine`.
pub fn db_integrity(wc: &Path) -> Result<()> {
    let c = open_ro(wc)?;
    let ic: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| err(format!("integrity_check: {e}")))?;
    if ic != "ok" {
        return Err(err(format!("wc.db integrity_check: {ic}")));
    }
    let fk: i64 = c
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .map_err(|e| err(format!("foreign_key_check: {e}")))?;
    if fk != 0 {
        return Err(err(format!("wc.db foreign_key_check: {fk} violations")));
    }
    let orphans: i64 = c
        .query_row(
            "SELECT count(*) FROM nodes n WHERE n.local_relpath <> '' AND NOT EXISTS \
             (SELECT 1 FROM nodes p WHERE p.wc_id = n.wc_id AND p.local_relpath = n.parent_relpath)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| err(format!("orphan query: {e}")))?;
    if orphans != 0 {
        return Err(err(format!(
            "wc.db: {orphans} NODES rows without a parent row"
        )));
    }
    let mut stmt = c
        .prepare(
            "SELECT DISTINCT checksum FROM nodes WHERE checksum IS NOT NULL \
             AND presence IN ('normal', 'incomplete')",
        )
        .map_err(|e| err(format!("checksum query: {e}")))?;
    let sums: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| err(format!("checksum query: {e}")))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| err(format!("checksum query: {e}")))?;
    for s in sums {
        let Some(hex) = s.strip_prefix("$sha1$") else {
            return Err(err(format!("NODES checksum {s:?} is not a SHA-1 checksum")));
        };
        let in_table: i64 = c
            .query_row(
                "SELECT count(*) FROM pristine WHERE checksum = ?1",
                [&s],
                |r| r.get(0),
            )
            .map_err(|e| err(format!("pristine query: {e}")))?;
        let file = wc
            .join(".svn/pristine")
            .join(hex.get(..2).unwrap_or("??"))
            .join(format!("{hex}.svn-base"));
        if in_table == 0 || !file.is_file() {
            return Err(err(format!(
                "pristine {hex} referenced by NODES is missing (row {in_table}, file {})",
                file.is_file()
            )));
        }
    }
    Ok(())
}

/// No unfinished work queue item and no working copy lock row is left.
pub fn db_quiescent(wc: &Path) -> Result<()> {
    let c = open_ro(wc)?;
    let wq: i64 = c
        .query_row("SELECT count(*) FROM work_queue", [], |r| r.get(0))
        .map_err(|e| err(format!("work_queue: {e}")))?;
    let locks: i64 = c
        .query_row("SELECT count(*) FROM wc_lock", [], |r| r.get(0))
        .map_err(|e| err(format!("wc_lock: {e}")))?;
    if wq != 0 || locks != 0 {
        return Err(err(format!(
            "after recovery: {wq} work queue items, {locks} wc_lock rows left"
        )));
    }
    Ok(())
}

/// `.svn/tmp` holds nothing but our lock markers (which recovery removes with their rows).
pub fn tmp_clean(wc: &Path) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(wc.join(".svn/tmp")) else {
        return Ok(());
    };
    let left: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if left.is_empty() {
        Ok(())
    } else {
        Err(err(format!(".svn/tmp not empty after recovery: {left:?}")))
    }
}

fn text_char(s: NodeStatus) -> char {
    match s {
        NodeStatus::None | NodeStatus::Normal => ' ',
        NodeStatus::Added => 'A',
        NodeStatus::Deleted => 'D',
        NodeStatus::Modified => 'M',
        NodeStatus::Replaced => 'R',
        NodeStatus::Missing | NodeStatus::Incomplete => '!',
        NodeStatus::Obstructed => '~',
        NodeStatus::Unversioned => '?',
        NodeStatus::Ignored => 'I',
        NodeStatus::Conflicted => 'C',
        NodeStatus::Excluded => ' ',
        NodeStatus::External => 'X',
    }
}

/// The engine's status in the shape of [`crate::svn::Svn::status`].
pub fn engine_status_map(list: &[WcStatus]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for s in list {
        let props = match s.props {
            PropStatus::Modified => 'M',
            PropStatus::Conflicted => 'C',
            PropStatus::None | PropStatus::Normal => ' ',
        };
        let tree = if s.tree_conflict { 'C' } else { ' ' };
        let code: String = [text_char(s.text), props, tree].iter().collect();
        if code.trim().is_empty() {
            continue;
        }
        let path = if s.path.as_str().is_empty() {
            ".".to_owned()
        } else {
            s.path.as_str().to_owned()
        };
        map.insert(path, code);
    }
    map
}

/// Differences between two status maps, as readable lines (empty when they agree).
pub fn status_diff(
    left_name: &str,
    left: &BTreeMap<String, String>,
    right_name: &str,
    right: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (p, c) in left {
        match right.get(p) {
            Some(c2) if c2 == c => {}
            Some(c2) => out.push(format!("{p}: {left_name} {c:?} vs {right_name} {c2:?}")),
            None => out.push(format!("{p}: {left_name} {c:?}, {right_name} unmodified")),
        }
    }
    for (p, c) in right {
        if !left.contains_key(p) {
            out.push(format!("{p}: {left_name} unmodified, {right_name} {c:?}"));
        }
    }
    out
}
