//! Normalised dump of a working copy's `.svn/wc.db` for comparison.
//!
//! Normalisation: time/date columns are dropped, columns keep table order, rows are sorted,
//! blobs are hex-rendered. Two working copies produced by equivalent operations must dump to
//! identical text.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

/// Tables whose content is run-specific or transient.
const SKIPPED_TABLES: &[&str] = &["WC_LOCK", "WORK_QUEUE"];

/// Column names carrying timestamps (matched case-insensitively as substrings).
const TIME_COLUMN_MARKERS: &[&str] = &["time", "date"];

fn is_time_column(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    TIME_COLUMN_MARKERS.iter().any(|m| lower.contains(m))
}

/// Dump `<wc>/.svn/wc.db` to normalised text.
pub fn dump_wc_db(wc_root: &Path) -> Result<String> {
    dump_sqlite(&wc_root.join(".svn").join("wc.db"))
}

/// Dump any SQLite database file to normalised text (read-only open).
pub fn dump_sqlite(db_path: &Path) -> Result<String> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))?;
    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    tables.sort();

    let mut out = String::new();
    for table in tables
        .iter()
        .filter(|t| !SKIPPED_TABLES.contains(&t.to_ascii_uppercase().as_str()))
    {
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        let kept: Vec<(usize, &String)> = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !is_time_column(c))
            .collect();
        let _ = writeln!(
            out,
            "## {table} [{}]",
            kept.iter()
                .map(|(_, c)| c.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );

        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\""))?;
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = stmt.query([])?;
        while let Some(row) = cursor.next()? {
            let mut cells = Vec::with_capacity(kept.len());
            for (idx, _) in &kept {
                cells.push(render_cell(row.get_ref(*idx)?));
            }
            rows.push(cells.join("|"));
        }
        rows.sort();
        for r in rows {
            let _ = writeln!(out, "{r}");
        }
    }
    Ok(out)
}

fn render_cell(v: ValueRef<'_>) -> String {
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => {
            let mut s = String::with_capacity(b.len() * 2 + 2);
            s.push_str("x'");
            for byte in b {
                let _ = write!(s, "{byte:02x}");
            }
            s.push('\'');
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_db(path: &Path, rows: &[(&str, i64, i64)]) -> Result<()> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE NODES (local_relpath TEXT, revision INTEGER, last_mod_time INTEGER, payload BLOB);
             CREATE TABLE WORK_QUEUE (id INTEGER, work BLOB);
             INSERT INTO WORK_QUEUE VALUES (1, x'00');",
        )?;
        for (name, rev, time) in rows {
            conn.execute(
                "INSERT INTO NODES (local_relpath, revision, last_mod_time, payload) VALUES (?1, ?2, ?3, x'ff00')",
                rusqlite::params![name, rev, time],
            )?;
        }
        Ok(())
    }

    #[test]
    fn ignores_time_columns_row_order_and_transient_tables() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (a, b) = (tmp.path().join("a.db"), tmp.path().join("b.db"));
        make_db(&a, &[("x", 1, 111), ("y", 2, 222)])?;
        make_db(&b, &[("y", 2, 999), ("x", 1, 888)])?;
        let (da, db) = (dump_sqlite(&a)?, dump_sqlite(&b)?);
        assert_eq!(da, db);
        assert!(da.contains("x'ff00'"));
        assert!(!da.contains("last_mod_time"));
        assert!(!da.contains("WORK_QUEUE"));
        Ok(())
    }

    #[test]
    fn detects_real_differences() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (a, b) = (tmp.path().join("a.db"), tmp.path().join("b.db"));
        make_db(&a, &[("x", 1, 1)])?;
        make_db(&b, &[("x", 2, 1)])?;
        assert_ne!(dump_sqlite(&a)?, dump_sqlite(&b)?);
        Ok(())
    }

    #[test]
    fn missing_db_is_an_error() {
        assert!(dump_sqlite(Path::new("/nonexistent/wc.db")).is_err());
    }
}
