//! `app.sqlite`: settings, repository layout, history and account metadata.

use std::path::Path;

use opensvn_ipc::{Account, AccountMethod, Layout};
use rusqlite::{OptionalExtension, params};

use super::migrations::APP_MIGRATIONS;
use super::{Db, DbError, now_iso};

/// Handle to `app.sqlite`. Cheap to clone; clones share one connection.
#[derive(Clone)]
pub struct AppDb {
    pub(super) db: Db,
}

impl AppDb {
    /// Opens `app.sqlite` inside `dir`, creating directory and schema as needed.
    pub fn open(dir: &Path) -> Result<Self, DbError> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            db: Db::open(&dir.join("app.sqlite"), APP_MIGRATIONS)?,
        })
    }

    /// In-memory database for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        Ok(Self {
            db: Db::open_in_memory(APP_MIGRATIONS)?,
        })
    }

    pub(crate) fn conn_handle(&self) -> &Db {
        &self.db
    }

    /// All stored settings as `(key, json)` pairs.
    pub fn settings_all(&self) -> Result<Vec<(String, String)>, DbError> {
        self.db.with(|c| {
            let mut stmt = c.prepare("SELECT key, value FROM setting")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Upserts and deletes settings atomically: `Some(json)` stores, `None` removes the key.
    pub fn settings_apply(&self, changes: &[(String, Option<String>)]) -> Result<(), DbError> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            for (key, value) in changes {
                match value {
                    Some(v) => tx.execute(
                        "INSERT INTO setting (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        params![key, v],
                    )?,
                    None => tx.execute("DELETE FROM setting WHERE key = ?1", params![key])?,
                };
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Layout stored for a repository, `None` when never set (the caller treats it as auto).
    pub fn layout_get(&self, repo_uuid: &str) -> Result<Option<Layout>, DbError> {
        self.db.with(|c| {
            let row = c
                .query_row(
                    "SELECT mode, trunk, branches, tags FROM repo_layout WHERE repo_uuid = ?1",
                    params![repo_uuid],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((mode, trunk, branches, tags)) = row else {
                return Ok(None);
            };
            let list = |v: Option<String>| -> Result<Vec<String>, DbError> {
                serde_json::from_str(&v.unwrap_or_else(|| "[]".to_owned()))
                    .map_err(|e| DbError::Corrupt(format!("layout list: {e}")))
            };
            Ok(Some(match mode.as_str() {
                "auto" => Layout::Auto,
                "none" => Layout::None,
                "custom" => Layout::Custom {
                    trunk: trunk.unwrap_or_default(),
                    branches: list(branches)?,
                    tags: list(tags)?,
                },
                other => return Err(DbError::Corrupt(format!("layout mode {other}"))),
            }))
        })
    }

    /// Stores the layout of a repository.
    pub fn layout_set(&self, repo_uuid: &str, layout: &Layout) -> Result<(), DbError> {
        let (mode, trunk, branches, tags) = match layout {
            Layout::Auto => ("auto", None, None, None),
            Layout::None => ("none", None, None, None),
            Layout::Custom {
                trunk,
                branches,
                tags,
            } => (
                "custom",
                Some(trunk.clone()),
                Some(json_list(branches)?),
                Some(json_list(tags)?),
            ),
        };
        self.db.with(|c| {
            c.execute(
                "INSERT INTO repo_layout (repo_uuid, mode, trunk, branches, tags)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(repo_uuid) DO UPDATE SET mode = excluded.mode,
                   trunk = excluded.trunk, branches = excluded.branches, tags = excluded.tags",
                params![repo_uuid, mode, trunk, branches, tags],
            )?;
            Ok(())
        })
    }

    /// Remembers a repository URL typed or opened by the user.
    pub fn recent_url_touch(&self, url: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO recent_url (url, last_used_at) VALUES (?1, ?2)
                 ON CONFLICT(url) DO UPDATE SET last_used_at = excluded.last_used_at",
                params![url, now_iso()],
            )?;
            Ok(())
        })
    }

    /// Repository URLs used recently, newest first (checkout dialog suggestions).
    pub fn recent_url_list(&self, limit: u32) -> Result<Vec<String>, DbError> {
        self.db.with(|c| {
            let mut stmt =
                c.prepare("SELECT url FROM recent_url ORDER BY last_used_at DESC, url LIMIT ?1")?;
            let rows = stmt
                .query_map(params![limit], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
    }

    /// Records a commit message for the history drop-down; the table keeps the newest 200 rows.
    pub fn commit_message_add(&self, repo_uuid: &str, message: &str) -> Result<(), DbError> {
        if message.trim().is_empty() {
            return Ok(());
        }
        self.db.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO commit_message_history (repo_uuid, message, used_at) VALUES (?1, ?2, ?3)",
                params![repo_uuid, message, now_iso()],
            )?;
            tx.execute(
                "DELETE FROM commit_message_history WHERE id NOT IN
                 (SELECT id FROM commit_message_history ORDER BY id DESC LIMIT 200)",
                [],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Account metadata (never passwords), newest use first.
    pub fn accounts_list(&self) -> Result<Vec<Account>, DbError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT realm, username, method, last_used_at FROM account
                 ORDER BY last_used_at DESC, host, port, realm",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(realm, username, method, last_used_at)| {
                    let method = match method.as_str() {
                        "basic" => AccountMethod::Basic,
                        "negotiate" => AccountMethod::Negotiate,
                        other => return Err(DbError::Corrupt(format!("account method {other}"))),
                    };
                    Ok(Account {
                        realm,
                        username,
                        method,
                        last_used_at,
                    })
                })
                .collect()
        })
    }
}

fn json_list(items: &[String]) -> Result<String, DbError> {
    serde_json::to_string(items).map_err(|e| DbError::Corrupt(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn settings_upsert_and_delete_are_atomic() {
        let db = AppDb::open_in_memory().unwrap();
        db.settings_apply(&[
            ("a".into(), Some("1".into())),
            ("b".into(), Some("\"x\"".into())),
        ])
        .unwrap();
        db.settings_apply(&[("a".into(), Some("2".into())), ("b".into(), None)])
            .unwrap();
        assert_eq!(db.settings_all().unwrap(), vec![("a".into(), "2".into())]);
    }

    #[test]
    fn layout_round_trips_every_mode() {
        let db = AppDb::open_in_memory().unwrap();
        assert_eq!(db.layout_get("u").unwrap(), None);
        for layout in [
            Layout::Auto,
            Layout::None,
            Layout::Custom {
                trunk: "/{project}/trunk".into(),
                branches: vec!["/{project}/branches".into()],
                tags: vec![],
            },
        ] {
            db.layout_set("u", &layout).unwrap();
            assert_eq!(db.layout_get("u").unwrap(), Some(layout));
        }
    }

    #[test]
    fn commit_history_keeps_newest_200_and_skips_blank() {
        let db = AppDb::open_in_memory().unwrap();
        db.commit_message_add("u", "  ").unwrap();
        for i in 0..205 {
            db.commit_message_add("u", &format!("m{i}")).unwrap();
        }
        let n: i64 = db
            .db
            .with(|c| {
                Ok(
                    c.query_row("SELECT count(*) FROM commit_message_history", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(n, 200);
    }

    #[test]
    fn opens_on_disk_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let db = AppDb::open(&dir.path().join("nested")).unwrap();
        db.recent_url_touch("https://svn.test.local/r").unwrap();
        db.recent_url_touch("https://svn.test.local/r").unwrap();
        assert_eq!(
            db.recent_url_list(10).unwrap(),
            ["https://svn.test.local/r"]
        );
        drop(db);
        AppDb::open(&dir.path().join("nested")).unwrap();
    }
}
