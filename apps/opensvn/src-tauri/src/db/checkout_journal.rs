//! `checkout_journal` table: checkouts that started and have not finished.

use rusqlite::{OptionalExtension, params};

use super::{AppDb, DbError, now_iso};

/// What the user chose for a checkout, kept until it finishes so that an interrupted checkout
/// is continued the same way. The revision and the depths live in the working copy (wc.db).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutJournal {
    /// Canonical absolute root of the new working copy.
    pub root_path: String,
    /// URL that was checked out.
    pub url: String,
    /// Requested depth (`infinity`, `immediates`, `files`, `empty`).
    pub depth: String,
    /// `--ignore-externals`.
    pub ignore_externals: bool,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CheckoutJournal> {
    Ok(CheckoutJournal {
        root_path: r.get(0)?,
        url: r.get(1)?,
        depth: r.get(2)?,
        ignore_externals: r.get::<_, i64>(3)? != 0,
    })
}

impl AppDb {
    /// Records a checkout that is about to start (replacing an older record of the same root).
    pub fn checkout_journal_put(&self, j: &CheckoutJournal) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO checkout_journal
                   (root_path, url, depth, ignore_externals, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    j.root_path,
                    j.url,
                    j.depth,
                    i64::from(j.ignore_externals),
                    now_iso()
                ],
            )?;
            Ok(())
        })
    }

    /// The unfinished checkout of `root_path`, if any.
    pub fn checkout_journal_get(
        &self,
        root_path: &str,
    ) -> Result<Option<CheckoutJournal>, DbError> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT root_path, url, depth, ignore_externals FROM checkout_journal
                 WHERE root_path = ?1",
                params![root_path],
                row,
            )
            .optional()?)
        })
    }

    /// Every unfinished checkout, oldest first.
    pub fn checkout_journal_list(&self) -> Result<Vec<CheckoutJournal>, DbError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT root_path, url, depth, ignore_externals FROM checkout_journal
                 ORDER BY started_at",
            )?;
            let rows = stmt.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// The checkout of `root_path` finished (or its folder holds no working copy any more).
    pub fn checkout_journal_remove(&self, root_path: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "DELETE FROM checkout_journal WHERE root_path = ?1",
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
    fn a_journal_row_lives_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let db = AppDb::open(dir.path()).unwrap();
        let j = CheckoutJournal {
            root_path: "/w/project".into(),
            url: "https://svn.example.com/repo/trunk".into(),
            depth: "infinity".into(),
            ignore_externals: true,
        };
        db.checkout_journal_put(&j).unwrap();
        assert_eq!(
            db.checkout_journal_get("/w/project").unwrap(),
            Some(j.clone())
        );
        assert_eq!(db.checkout_journal_list().unwrap(), vec![j.clone()]);
        let again = CheckoutJournal {
            ignore_externals: false,
            ..j
        };
        db.checkout_journal_put(&again).unwrap();
        assert_eq!(db.checkout_journal_list().unwrap(), vec![again]);
        db.checkout_journal_remove("/w/project").unwrap();
        assert!(db.checkout_journal_get("/w/project").unwrap().is_none());
    }
}
