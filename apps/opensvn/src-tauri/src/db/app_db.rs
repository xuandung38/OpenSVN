//! `app.sqlite`: settings, repository layout, history and account metadata.

use std::path::Path;

use opensvn_ipc::{Account, AccountMethod, Layout, TrustedCert, TrustedCertKey};
use rusqlite::{OptionalExtension, params};

use super::migrations::APP_MIGRATIONS;
use super::{Db, DbError, now_iso};

/// How many recent repository URLs `app.sqlite` keeps.
pub const RECENT_URL_CAP: u32 = 20;

/// One stored setting of the `app.sqlite` in `dir`, read without creating or migrating anything
/// (the app reads it before it starts, see `startup_env`). `None` when the file, the table or the
/// key is missing, or the value is not JSON.
pub fn stored_setting(dir: &Path, key: &str) -> Option<serde_json::Value> {
    use rusqlite::OpenFlags;
    let path = dir.join("app.sqlite");
    if !path.is_file() {
        return None;
    }
    let c = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let raw: String = c
        .query_row("SELECT value FROM setting WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()
        .ok()??;
    serde_json::from_str(&raw).ok()
}

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

    /// Remembers a repository URL typed or opened by the user; the table keeps the newest
    /// [`RECENT_URL_CAP`] rows.
    pub fn recent_url_touch(&self, url: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO recent_url (url, last_used_at) VALUES (?1, ?2)
                 ON CONFLICT(url) DO UPDATE SET last_used_at = excluded.last_used_at",
                params![url, now_iso()],
            )?;
            tx.execute(
                "DELETE FROM recent_url WHERE url NOT IN
                 (SELECT url FROM recent_url ORDER BY last_used_at DESC, url LIMIT ?1)",
                params![RECENT_URL_CAP],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Forgets a recent repository URL (nothing happens when it is not listed).
    pub fn recent_url_remove(&self, url: &str) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute("DELETE FROM recent_url WHERE url = ?1", params![url])?;
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

/// An account of a realm, as [`AppDb::accounts_of_realm`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedAccount {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
}

/// The saved account of one origin and realm, as [`AppDb::account_get`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedAccount {
    pub username: Option<String>,
    pub negotiate: bool,
    /// The server rejected the saved password; it must not be offered automatically.
    pub stale: bool,
}

/// Origin and account fields stored for a saved credential.
pub struct AccountRecord<'a> {
    pub scheme: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub realm: &'a str,
    pub username: &'a str,
    pub negotiate: bool,
}

impl AppDb {
    /// Records (or refreshes) the metadata of a saved credential and clears its stale flag (a
    /// new password was saved). Never stores a password.
    pub fn account_upsert(&self, a: &AccountRecord<'_>) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO account (scheme, host, port, realm, username, method, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(scheme, host, port, realm) DO UPDATE SET
                   username = excluded.username, method = excluded.method,
                   last_used_at = excluded.last_used_at, stale = 0",
                params![
                    a.scheme,
                    a.host,
                    a.port,
                    a.realm,
                    a.username,
                    if a.negotiate { "negotiate" } else { "basic" },
                    now_iso()
                ],
            )?;
            Ok(())
        })
    }

    /// The saved account of one origin and realm.
    pub fn account_get(
        &self,
        scheme: &str,
        host: &str,
        port: u16,
        realm: &str,
    ) -> Result<Option<SavedAccount>, DbError> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT username, method, stale FROM account
                 WHERE scheme = ?1 AND host = ?2 AND port = ?3 AND realm = ?4",
                params![scheme, host, port, realm],
                |r| {
                    Ok(SavedAccount {
                        username: r.get::<_, Option<String>>(0)?,
                        negotiate: r.get::<_, String>(1)? == "negotiate",
                        stale: r.get::<_, i64>(2)? != 0,
                    })
                },
            )
            .optional()?)
        })
    }

    /// Marks the saved password of `username` at one origin and realm as rejected by the
    /// server. Returns whether such an account exists. Only [`Self::account_upsert`] (a newly
    /// saved password) clears the flag.
    pub fn account_mark_stale(
        &self,
        scheme: &str,
        host: &str,
        port: u16,
        realm: &str,
        username: &str,
    ) -> Result<bool, DbError> {
        self.db.with(|c| {
            Ok(c.execute(
                "UPDATE account SET stale = 1
                 WHERE scheme = ?1 AND host = ?2 AND port = ?3 AND realm = ?4 AND username = ?5",
                params![scheme, host, port, realm, username],
            )? > 0)
        })
    }

    /// The accounts of a realm, as `(scheme, host, port, username)`.
    pub fn accounts_of_realm(&self, realm: &str) -> Result<Vec<RemovedAccount>, DbError> {
        self.db.with(|c| {
            let mut stmt =
                c.prepare("SELECT scheme, host, port, username FROM account WHERE realm = ?1")?;
            Ok(stmt
                .query_map(params![realm], |r| {
                    Ok(RemovedAccount {
                        scheme: r.get(0)?,
                        host: r.get(1)?,
                        port: r.get(2)?,
                        username: r.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Removes the account of one origin and realm; returns whether it existed.
    pub fn account_delete(
        &self,
        scheme: &str,
        host: &str,
        port: u16,
        realm: &str,
    ) -> Result<bool, DbError> {
        self.db.with(|c| {
            Ok(c.execute(
                "DELETE FROM account WHERE scheme = ?1 AND host = ?2 AND port = ?3 AND realm = ?4",
                params![scheme, host, port, realm],
            )? > 0)
        })
    }

    /// Whether this exact certificate was trusted permanently for this host and port.
    pub fn cert_is_trusted(&self, host: &str, port: u16, sha256: &str) -> Result<bool, DbError> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM trusted_cert WHERE host = ?1 AND port = ?2 AND sha256 = ?3",
                params![host, port, sha256],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
    }

    /// Permanently trusted certificates, newest first.
    pub fn certs_list(&self) -> Result<Vec<TrustedCert>, DbError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT host, port, sha256, subject, expires_at, trusted_at FROM trusted_cert
                 ORDER BY trusted_at DESC, host, port",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(TrustedCert {
                        host: r.get(0)?,
                        port: r.get(1)?,
                        sha256: r.get(2)?,
                        subject: r.get(3)?,
                        expires_at: r.get(4)?,
                        trusted_at: r.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Stops trusting a certificate; returns whether it was trusted.
    pub fn certs_remove(&self, key: &TrustedCertKey) -> Result<bool, DbError> {
        self.db.with(|c| {
            Ok(c.execute(
                "DELETE FROM trusted_cert WHERE host = ?1 AND port = ?2 AND sha256 = ?3",
                params![key.host, key.port, key.sha256],
            )? > 0)
        })
    }

    /// Replaces every pinned certificate of `host:port` with the given one (the user accepted a
    /// renewed certificate).
    pub fn cert_replace_pin(
        &self,
        host: &str,
        port: u16,
        sha256: &str,
        subject: &str,
        expires_at: &str,
    ) -> Result<(), DbError> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM trusted_cert WHERE host = ?1 AND port = ?2",
                params![host, port],
            )?;
            tx.execute(
                "INSERT INTO trusted_cert (host, port, sha256, subject, expires_at, trusted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![host, port, sha256, subject, expires_at, now_iso()],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Trusts a certificate permanently for one host and port.
    pub fn cert_trust(
        &self,
        host: &str,
        port: u16,
        sha256: &str,
        subject: &str,
        expires_at: &str,
    ) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO trusted_cert (host, port, sha256, subject, expires_at, trusted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(host, port, sha256) DO UPDATE SET trusted_at = excluded.trusted_at",
                params![host, port, sha256, subject, expires_at, now_iso()],
            )?;
            Ok(())
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
    fn a_stored_setting_is_read_before_start_without_creating_anything() {
        let dir = tempfile::tempdir().unwrap();
        let key = "appearance.hardwareAcceleration";
        assert_eq!(stored_setting(dir.path(), key), None);
        assert!(!dir.path().join("app.sqlite").exists(), "nothing created");
        let db = AppDb::open(dir.path()).unwrap();
        assert_eq!(stored_setting(dir.path(), key), None);
        db.settings_apply(&[(key.into(), Some("false".into()))])
            .unwrap();
        assert_eq!(
            stored_setting(dir.path(), key),
            Some(serde_json::Value::Bool(false))
        );
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
    fn recent_urls_are_capped_and_removable() {
        let db = AppDb::open_in_memory().unwrap();
        for i in 0..RECENT_URL_CAP + 5 {
            db.recent_url_touch(&format!("https://svn.test.local/r{i:02}"))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let n: u32 = db
            .db
            .with(|c| Ok(c.query_row("SELECT count(*) FROM recent_url", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, RECENT_URL_CAP);
        let list = db.recent_url_list(100).unwrap();
        assert_eq!(
            list[0],
            format!("https://svn.test.local/r{:02}", RECENT_URL_CAP + 4)
        );
        assert!(!list.contains(&"https://svn.test.local/r00".to_owned()));
        db.recent_url_remove(&list[0]).unwrap();
        db.recent_url_remove("https://svn.test.local/never")
            .unwrap();
        let after = db.recent_url_list(100).unwrap();
        assert_eq!(after.len(), list.len() - 1);
        assert!(!after.contains(&list[0]));
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
