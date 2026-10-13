//! `working_copy` table: the registry of working copies the user added.

use rusqlite::{OptionalExtension, params};

use super::{AppDb, DbError, now_iso};

/// Persisted registry row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WcRow {
    /// UUID v7 id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Canonical absolute root path.
    pub root_path: String,
    /// Repository UUID.
    pub repo_uuid: String,
    /// Repository root URL.
    pub repo_root_url: String,
    /// Position in the sidebar.
    pub sort_order: i64,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WcRow> {
    Ok(WcRow {
        id: r.get(0)?,
        name: r.get(1)?,
        root_path: r.get(2)?,
        repo_uuid: r.get(3)?,
        repo_root_url: r.get(4)?,
        sort_order: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
    })
}

const COLS: &str = "id, name, root_path, repo_uuid, repo_root_url, sort_order";

impl AppDb {
    /// Registered working copies in sidebar order.
    pub fn wc_list(&self) -> Result<Vec<WcRow>, DbError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLS} FROM working_copy ORDER BY sort_order, added_at"
            ))?;
            let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Looks a working copy up by id.
    pub fn wc_get(&self, id: &str) -> Result<Option<WcRow>, DbError> {
        self.db.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM working_copy WHERE id = ?1"),
                params![id],
                row,
            )
            .optional()?)
        })
    }

    /// Looks a working copy up by its root path.
    pub fn wc_get_by_root(&self, root_path: &str) -> Result<Option<WcRow>, DbError> {
        self.db.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM working_copy WHERE root_path = ?1"),
                params![root_path],
                row,
            )
            .optional()?)
        })
    }

    /// Adds a working copy at the end of the list.
    pub fn wc_insert(&self, r: &WcRow) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO working_copy
                   (id, name, root_path, repo_uuid, repo_root_url, sort_order, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5,
                   (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM working_copy), ?6)",
                params![
                    r.id,
                    r.name,
                    r.root_path,
                    r.repo_uuid,
                    r.repo_root_url,
                    now_iso()
                ],
            )?;
            Ok(())
        })
    }

    /// Removes the registry row (files on disk are untouched). Returns whether a row existed.
    pub fn wc_remove(&self, id: &str) -> Result<bool, DbError> {
        self.db
            .with(|c| Ok(c.execute("DELETE FROM working_copy WHERE id = ?1", params![id])? > 0))
    }

    /// Renames a working copy. Returns whether a row existed.
    pub fn wc_rename(&self, id: &str, name: &str) -> Result<bool, DbError> {
        self.db.with(|c| {
            Ok(c.execute(
                "UPDATE working_copy SET name = ?2 WHERE id = ?1",
                params![id, name],
            )? > 0)
        })
    }

    /// Stamps the time a working copy was opened.
    pub fn wc_touch_opened(&self, id: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "UPDATE working_copy SET last_opened_at = ?2 WHERE id = ?1",
                params![id, now_iso()],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn sample(id: &str, root: &str) -> WcRow {
        WcRow {
            id: id.into(),
            name: id.into(),
            root_path: root.into(),
            repo_uuid: "u".into(),
            repo_root_url: "https://svn.test.local/r".into(),
            sort_order: 0,
        }
    }

    #[test]
    fn registry_crud_keeps_order_and_unique_roots() {
        let db = AppDb::open_in_memory().unwrap();
        db.wc_insert(&sample("a", "/wc/a")).unwrap();
        db.wc_insert(&sample("b", "/wc/b")).unwrap();
        assert!(
            db.wc_insert(&sample("c", "/wc/a")).is_err(),
            "duplicate root"
        );
        let ids: Vec<_> = db.wc_list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(db.wc_rename("a", "Alpha").unwrap());
        assert_eq!(db.wc_get("a").unwrap().unwrap().name, "Alpha");
        assert_eq!(db.wc_get_by_root("/wc/b").unwrap().unwrap().id, "b");
        assert!(db.wc_remove("a").unwrap());
        assert!(!db.wc_remove("a").unwrap());
        assert_eq!(db.wc_list().unwrap().len(), 1);
    }
}
