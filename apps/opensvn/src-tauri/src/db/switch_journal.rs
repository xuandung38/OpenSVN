//! `switch_journal` table: switches that started and have not finished.

use rusqlite::{OptionalExtension, params};

use super::{AppDb, DbError, now_iso};

impl AppDb {
    /// Records that the working copy at `root_path` is being switched to `url` (replacing an
    /// older record of the same root).
    pub fn switch_journal_put(&self, root_path: &str, url: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO switch_journal (root_path, url, started_at)
                 VALUES (?1, ?2, ?3)",
                params![root_path, url, now_iso()],
            )?;
            Ok(())
        })
    }

    /// URL of the unfinished switch of `root_path`, if any.
    pub fn switch_journal_get(&self, root_path: &str) -> Result<Option<String>, DbError> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT url FROM switch_journal WHERE root_path = ?1",
                params![root_path],
                |r| r.get(0),
            )
            .optional()?)
        })
    }

    /// The switch of `root_path` finished, or the working copy is complete again.
    pub fn switch_journal_remove(&self, root_path: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "DELETE FROM switch_journal WHERE root_path = ?1",
                params![root_path],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_row_lives_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let db = AppDb::open(dir.path()).unwrap();
        db.switch_journal_put("/w/project", "https://svn.example.com/repo/branches/a")
            .unwrap();
        db.switch_journal_put("/w/project", "https://svn.example.com/repo/branches/b")
            .unwrap();
        assert_eq!(
            db.switch_journal_get("/w/project").unwrap().as_deref(),
            Some("https://svn.example.com/repo/branches/b")
        );
        assert!(db.switch_journal_get("/w/other").unwrap().is_none());
        db.switch_journal_remove("/w/project").unwrap();
        assert!(db.switch_journal_get("/w/project").unwrap().is_none());
    }
}
