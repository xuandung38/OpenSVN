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
pub const APP_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("migrations/app_0001_initial.sql"),
}];

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
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let latest = migrations.last().map_or(0, |m| m.version);
    if current > latest {
        return Err(DbError::NewerSchema {
            found: current,
            supported: latest,
        });
    }
    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
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
        assert_eq!(version(&c), 1);
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
                supported: 1
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
