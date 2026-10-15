//! Versioned schema migrations driven by `PRAGMA user_version`.

use rusqlite::Connection;

use super::DbError;

/// One forward-only migration. `version` values are strictly increasing, starting at 1.
pub struct Migration {
    /// Schema version after this migration ran.
    pub version: u32,
    /// SQL batch executed inside a transaction.
    pub sql: &'static str,
}

/// Migrations of `app.sqlite`.
pub const APP_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("migrations/app_0001_initial.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("migrations/app_0002_account_stale.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("migrations/app_0003_checkout_journal.sql"),
    },
];

/// Migrations of `cache.sqlite`.
pub const CACHE_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("migrations/cache_0001_initial.sql"),
}];

/// Brings `conn` to the newest version in `migrations`.
///
/// Each migration and its version bump commit atomically. A database written by a newer app
/// (higher `user_version`) is refused instead of being silently downgraded.
pub fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<(), DbError> {
    let latest = migrations.last().map_or(0, |m| m.version);
    let probe: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if probe > latest {
        return Err(DbError::NewerSchema {
            found: probe,
            supported: latest,
        });
    }
    for m in migrations {
        // IMMEDIATE takes the write lock first; the version is read again inside it, so a second
        // instance that started at the same time sees the step already applied and skips it.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: u32 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current >= m.version {
            continue;
        }
        tx.execute_batch(m.sql)?;
        // `PRAGMA user_version = N` does not accept bound parameters.
        tx.execute_batch(&format!("PRAGMA user_version = {}", m.version))?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn version(c: &Connection) -> u32 {
        c.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn applies_all_and_is_idempotent() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, APP_MIGRATIONS).unwrap();
        assert_eq!(version(&c), 3);
        migrate(&mut c, APP_MIGRATIONS).unwrap();
        c.execute("INSERT INTO setting (key, value) VALUES ('a', '1')", [])
            .unwrap();
    }

    #[test]
    fn upgrades_stepwise_and_rolls_back_a_failing_step() {
        let mut c = Connection::open_in_memory().unwrap();
        let steps = [
            Migration {
                version: 1,
                sql: "CREATE TABLE t (a INTEGER);",
            },
            Migration {
                version: 2,
                sql: "ALTER TABLE t ADD COLUMN b TEXT;",
            },
        ];
        migrate(&mut c, &steps[..1]).unwrap();
        migrate(&mut c, &steps).unwrap();
        assert_eq!(version(&c), 2);

        let bad = [Migration {
            version: 3,
            sql: "CREATE TABLE u (x); THIS IS NOT SQL;",
        }];
        assert!(migrate(&mut c, &bad).is_err());
        assert_eq!(version(&c), 2);
        let exists: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'u'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "failed migration must roll back");
    }

    #[test]
    fn refuses_database_from_newer_app() {
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA user_version = 99").unwrap();
        assert!(matches!(
            migrate(&mut c, APP_MIGRATIONS),
            Err(DbError::NewerSchema {
                found: 99,
                supported: 3
            })
        ));
    }

    #[test]
    fn account_is_keyed_by_origin_and_trusted_cert_by_port() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, APP_MIGRATIONS).unwrap();
        let ins = "INSERT INTO account (scheme, host, port, realm, method) VALUES (?1, ?2, ?3, 'r', 'basic')";
        c.execute(ins, rusqlite::params!["https", "h", 443])
            .unwrap();
        c.execute(ins, rusqlite::params!["https", "h", 8443])
            .unwrap();
        assert!(
            c.execute(ins, rusqlite::params!["https", "h", 443])
                .is_err()
        );
        let cert =
            "INSERT INTO trusted_cert (host, port, sha256, trusted_at) VALUES ('h', ?1, 'ab', 't')";
        c.execute(cert, [443]).unwrap();
        c.execute(cert, [8443]).unwrap();
        assert!(c.execute(cert, [443]).is_err());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod upgrade_tests {
    use super::*;

    #[test]
    fn version_1_accounts_survive_and_start_not_stale() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, &APP_MIGRATIONS[..1]).unwrap();
        c.execute(
            "INSERT INTO account (scheme, host, port, realm, username, method)
             VALUES ('https', 'h', 443, 'r', 'u', 'basic')",
            [],
        )
        .unwrap();
        migrate(&mut c, APP_MIGRATIONS).unwrap();
        let stale: i64 = c
            .query_row("SELECT stale FROM account WHERE host = 'h'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stale, 0);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod race_tests {
    use super::*;

    #[test]
    fn two_instances_starting_together_both_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sqlite");
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut c = Connection::open(path).unwrap();
                    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                    migrate(&mut c, APP_MIGRATIONS)
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let c = Connection::open(&path).unwrap();
        let v: u32 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
    }
}
