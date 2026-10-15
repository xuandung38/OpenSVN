//! Normalised dump of a working copy's `.svn/wc.db` for comparison.
//!
//! What is compared: `user_version`; every index, trigger and view definition; all table rows
//! (sorted). What is normalised, and nothing else:
//! - `NODES.last_mod_time` is rendered as `SET` / `NULL` (it is the local file mtime, but a NULL
//!   changes svn's status fast path, so presence still matters);
//! - skel-encoded property columns are rendered canonically (sorted by property name), because
//!   svn writes them in per-process hash order;
//! - `WC_LOCK` and `WORK_QUEUE` are rendered as row counts. Use [`work_queue_rows`] to assert a
//!   completed operation left the queue empty.
//!
//! Server-sourced dates (`changed_date`, `lock_date`) are intentionally kept.
//!
//! [`DumpOptions::mask_dav_cache`] (scenario `ignore: [dav_cache]`) also renders
//! `NODES.dav_cache` as `MASKED`: the RA layer's wcprop cache, which svn's ra_serf fills with
//! `svn:wc:ra_dav:version-url` and the OpenSVN engine leaves empty; svn fetches whatever it
//! needs from the server when the cache is empty.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::skel;

/// Tables rendered as `rows=<n>` instead of their content.
const COUNT_ONLY_TABLES: &[&str] = &["WC_LOCK", "WORK_QUEUE"];

/// Columns holding skel-encoded property data (table, column).
const SKEL_COLUMNS: &[(&str, &str)] = &[
    ("NODES", "properties"),
    ("NODES", "dav_cache"),
    ("NODES", "inherited_props"),
    ("ACTUAL_NODE", "properties"),
];

/// Columns rendered as `SET` / `NULL` (table, column).
const PRESENCE_ONLY_COLUMNS: &[(&str, &str)] = &[("NODES", "last_mod_time")];

fn listed(list: &[(&str, &str)], table: &str, column: &str) -> bool {
    list.iter()
        .any(|(t, c)| t.eq_ignore_ascii_case(table) && c.eq_ignore_ascii_case(column))
}

/// What a dump leaves out besides the fixed normalisations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DumpOptions {
    /// Render `NODES.dav_cache` as `MASKED`.
    pub mask_dav_cache: bool,
}

/// Dump `<wc>/.svn/wc.db` to normalised text.
pub fn dump_wc_db(wc_root: &Path) -> Result<String> {
    dump_wc_db_with(wc_root, DumpOptions::default())
}

/// [`dump_wc_db`] with options.
pub fn dump_wc_db_with(wc_root: &Path, opts: DumpOptions) -> Result<String> {
    dump_sqlite_with(&wc_root.join(".svn").join("wc.db"), opts)
}

fn open_read_only(db_path: &Path) -> Result<Connection> {
    Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", db_path.display()))
}

/// Number of rows in `WORK_QUEUE` of `<wc>/.svn/wc.db` (0 after a completed operation).
pub fn work_queue_rows(wc_root: &Path) -> Result<u64> {
    let conn = open_read_only(&wc_root.join(".svn").join("wc.db"))?;
    Ok(conn
        .query_row("SELECT COUNT(*) FROM WORK_QUEUE", [], |r| {
            r.get::<_, i64>(0)
        })?
        .unsigned_abs())
}

/// Dump any SQLite database file to normalised text (read-only open).
pub fn dump_sqlite(db_path: &Path) -> Result<String> {
    dump_sqlite_with(db_path, DumpOptions::default())
}

fn dump_sqlite_with(db_path: &Path, opts: DumpOptions) -> Result<String> {
    let conn = open_read_only(db_path)?;
    let mut out = String::new();
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let _ = writeln!(out, "## user_version {user_version}");

    // Schema objects other than tables, whitespace-normalised and sorted.
    let mut schema: Vec<String> = conn
        .prepare(
            "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_master \
             WHERE type IN ('index','trigger','view')",
        )?
        .query_map([], |r| {
            let sql: String = r.get(3)?;
            Ok(format!(
                "{} {} on {}: {}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                sql.split_whitespace().collect::<Vec<_>>().join(" ")
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    schema.sort();
    for line in schema {
        let _ = writeln!(out, "## schema {line}");
    }

    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    tables.sort();

    for table in &tables {
        let columns: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        let _ = writeln!(out, "## {table} [{}]", columns.join(","));

        if COUNT_ONLY_TABLES
            .iter()
            .any(|t| t.eq_ignore_ascii_case(table))
        {
            let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })?;
            let _ = writeln!(out, "rows={n}");
            continue;
        }

        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\""))?;
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = stmt.query([])?;
        while let Some(row) = cursor.next()? {
            let mut cells = Vec::with_capacity(columns.len());
            for (idx, column) in columns.iter().enumerate() {
                if opts.mask_dav_cache
                    && table.eq_ignore_ascii_case("NODES")
                    && column.eq_ignore_ascii_case("dav_cache")
                {
                    cells.push("MASKED".into());
                } else {
                    cells.push(render_cell(table, column, row.get_ref(idx)?));
                }
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

fn render_cell(table: &str, column: &str, v: ValueRef<'_>) -> String {
    if listed(PRESENCE_ONLY_COLUMNS, table, column) {
        return if matches!(v, ValueRef::Null) {
            "NULL"
        } else {
            "SET"
        }
        .into();
    }
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) if listed(SKEL_COLUMNS, table, column) => skel::canonical(b),
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

    /// (relpath, revision, last_mod_time, changed_date, properties)
    type Row<'a> = (&'a str, i64, Option<i64>, i64, &'a [u8]);

    const SCHEMA: &str = "CREATE TABLE NODES (local_relpath TEXT, revision INTEGER, last_mod_time INTEGER, changed_date INTEGER, properties BLOB);
         CREATE INDEX I_NODES ON NODES (local_relpath);
         CREATE TABLE WORK_QUEUE (id INTEGER, work BLOB);
         CREATE TABLE WC_LOCK (id INTEGER);";

    fn make_db(path: &Path, rows: &[Row<'_>]) -> Result<()> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        for (name, rev, mtime, date, props) in rows {
            conn.execute(
                "INSERT INTO NODES VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![name, rev, mtime, date, props],
            )?;
        }
        Ok(())
    }

    const PROPS_A: &[u8] = b"(svn:eol-style native svn:keywords 6 Id Rev svn:needs-lock 1 *)";
    const PROPS_B: &[u8] = b"(svn:needs-lock 1 * svn:eol-style native svn:keywords 6 Id Rev)";

    #[test]
    fn property_byte_order_and_mtime_value_are_ignored() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (a, b) = (tmp.path().join("a.db"), tmp.path().join("b.db"));
        make_db(
            &a,
            &[
                ("x", 1, Some(111), 5, PROPS_A),
                ("y", 2, Some(1), 6, PROPS_A),
            ],
        )?;
        make_db(
            &b,
            &[
                ("y", 2, Some(999), 6, PROPS_B),
                ("x", 1, Some(888), 5, PROPS_B),
            ],
        )?;
        let (da, db) = (dump_sqlite(&a)?, dump_sqlite(&b)?);
        assert_eq!(da, db);
        assert!(da.contains("svn:eol-style=native"), "{da}");
        Ok(())
    }

    #[test]
    fn real_differences_are_detected() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let base = tmp.path().join("base.db");
        make_db(&base, &[("x", 1, Some(1), 5, PROPS_A)])?;
        let expected = dump_sqlite(&base)?;
        // (rows for the second db, what changed)
        let cases: [(&str, [Row<'_>; 1]); 4] = [
            ("revision", [("x", 2, Some(1), 5, PROPS_A)]),
            ("changed_date is kept", [("x", 1, Some(1), 6, PROPS_A)]),
            ("NULL vs set mtime", [("x", 1, None, 5, PROPS_A)]),
            (
                "property value",
                [(
                    "x",
                    1,
                    Some(1),
                    5,
                    b"(svn:eol-style CRLF svn:keywords 6 Id Rev svn:needs-lock 1 *)",
                )],
            ),
        ];
        for (what, rows) in cases {
            let other = tmp.path().join("other.db");
            let _ = std::fs::remove_file(&other);
            make_db(&other, &rows)?;
            assert_ne!(dump_sqlite(&other)?, expected, "{what} must be detected");
        }
        Ok(())
    }

    #[test]
    fn schema_objects_user_version_and_work_queue_are_compared() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (a, b) = (tmp.path().join("a.db"), tmp.path().join("b.db"));
        make_db(&a, &[])?;
        make_db(&b, &[])?;
        assert_eq!(dump_sqlite(&a)?, dump_sqlite(&b)?);

        Connection::open(&b)?.execute_batch("CREATE INDEX I_EXTRA ON NODES (revision);")?;
        assert_ne!(dump_sqlite(&a)?, dump_sqlite(&b)?, "missing/extra index");
        let c = tmp.path().join("c.db");
        make_db(&c, &[])?;
        Connection::open(&c)?.execute_batch("PRAGMA user_version = 32;")?;
        assert_ne!(dump_sqlite(&a)?, dump_sqlite(&c)?, "user_version");
        let d = tmp.path().join("d.db");
        make_db(&d, &[])?;
        Connection::open(&d)?.execute_batch("INSERT INTO WORK_QUEUE VALUES (1, x'00');")?;
        assert_ne!(dump_sqlite(&a)?, dump_sqlite(&d)?, "work queue rows");
        Ok(())
    }

    #[test]
    fn work_queue_rows_counts_pending_work() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        std::fs::create_dir(tmp.path().join(".svn"))?;
        let db = tmp.path().join(".svn").join("wc.db");
        make_db(&db, &[])?;
        assert_eq!(work_queue_rows(tmp.path())?, 0);
        Connection::open(&db)?.execute_batch("INSERT INTO WORK_QUEUE VALUES (1, x'00');")?;
        assert_eq!(work_queue_rows(tmp.path())?, 1);
        Ok(())
    }

    #[test]
    fn missing_db_is_an_error() {
        assert!(dump_sqlite(Path::new("/nonexistent/wc.db")).is_err());
    }
}
