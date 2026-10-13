//! `cache.sqlite`: disposable cache. Losing the file only costs a rescan.

use std::path::Path;

use opensvn_ipc::{FileStatus, WcId};
use rusqlite::params;

use super::migrations::CACHE_MIGRATIONS;
use super::{Db, DbError};

/// Handle to `cache.sqlite`. Cheap to clone.
#[derive(Clone)]
pub struct CacheDb {
    db: Db,
}

impl CacheDb {
    /// Opens `cache.sqlite` inside `dir`. A cache that cannot be opened (corrupt, newer schema)
    /// is deleted and recreated because its content is always reproducible.
    pub fn open(dir: &Path) -> Result<Self, DbError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("cache.sqlite");
        match Db::open(&path, CACHE_MIGRATIONS) {
            Ok(db) => Ok(Self { db }),
            Err(e) => {
                tracing::warn!(error = %e, "cache database unusable; recreating");
                for suffix in ["", "-wal", "-shm"] {
                    let mut p = path.clone().into_os_string();
                    p.push(suffix);
                    let _ = std::fs::remove_file(p);
                }
                Ok(Self {
                    db: Db::open(&path, CACHE_MIGRATIONS)?,
                })
            }
        }
    }

    /// In-memory cache for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        Ok(Self {
            db: Db::open_in_memory(CACHE_MIGRATIONS)?,
        })
    }

    /// Replaces the stored status snapshot of a working copy.
    pub fn status_snapshot_replace(
        &self,
        wc_id: &WcId,
        statuses: &[FileStatus],
    ) -> Result<(), DbError> {
        let encoded = statuses
            .iter()
            .map(|s| {
                serde_json::to_vec(s)
                    .map(|b| (s.path.clone(), b))
                    .map_err(|e| DbError::Corrupt(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.db.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM status_snapshot WHERE wc_id = ?1",
                params![wc_id.0],
            )?;
            {
                let mut ins = tx.prepare(
                    "INSERT INTO status_snapshot (wc_id, path, data) VALUES (?1, ?2, ?3)",
                )?;
                for (path, data) in &encoded {
                    ins.execute(params![wc_id.0, path, data])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Stored snapshot, `None` when the working copy was never scanned. Undecodable rows (written
    /// by an older build) are skipped: the cache is advisory.
    pub fn status_snapshot_get(&self, wc_id: &WcId) -> Result<Option<Vec<FileStatus>>, DbError> {
        self.db.with(|c| {
            let mut stmt =
                c.prepare("SELECT data FROM status_snapshot WHERE wc_id = ?1 ORDER BY path")?;
            let blobs = stmt
                .query_map(params![wc_id.0], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if blobs.is_empty() {
                return Ok(None);
            }
            Ok(Some(
                blobs
                    .iter()
                    .filter_map(|b| serde_json::from_slice(b).ok())
                    .collect(),
            ))
        })
    }

    /// Drops the snapshot of a working copy (used when it is removed from the registry).
    pub fn status_snapshot_clear(&self, wc_id: &WcId) -> Result<(), DbError> {
        self.db.with(|c| {
            c.execute(
                "DELETE FROM status_snapshot WHERE wc_id = ?1",
                params![wc_id.0],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use opensvn_ipc::{FileKind, NodeStatus, PropStatus};

    fn status(path: &str) -> FileStatus {
        FileStatus {
            path: path.into(),
            kind: FileKind::File,
            text: NodeStatus::Modified,
            props: PropStatus::None,
            tree_conflict: false,
            moved_from: None,
            moved_to: None,
            changelist: None,
            lock: None,
            needs_lock: false,
            remote: None,
            binary: false,
            size: None,
        }
    }

    #[test]
    fn snapshot_round_trip_and_replace() {
        let db = CacheDb::open_in_memory().unwrap();
        let id = WcId("w".into());
        assert_eq!(db.status_snapshot_get(&id).unwrap(), None);
        db.status_snapshot_replace(&id, &[status("a"), status("b")])
            .unwrap();
        db.status_snapshot_replace(&id, &[status("c")]).unwrap();
        assert_eq!(
            db.status_snapshot_get(&id).unwrap(),
            Some(vec![status("c")])
        );
        db.status_snapshot_clear(&id).unwrap();
        assert_eq!(db.status_snapshot_get(&id).unwrap(), None);
    }

    #[test]
    fn corrupt_cache_file_is_recreated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cache.sqlite"), b"definitely not sqlite").unwrap();
        let db = CacheDb::open(dir.path()).unwrap();
        assert_eq!(db.status_snapshot_get(&WcId("w".into())).unwrap(), None);
    }
}
