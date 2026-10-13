//! `WcDb` implementations of the track B store traits: [`PristineIndex`] for the pristine
//! store, [`WorkQueueStore`] for the persistent work queue and [`WorkingCopyOps`] for the
//! database callbacks of work items, plus [`WcDb::pristine_store`] and
//! [`WcDb::run_work_queue`] that wire them up.
//!
//! Every method is safe to repeat after a crash, as the work queue contract requires.
//! `$Date$` keywords are expanded in UTC (this crate carries no time-zone database).

use std::collections::BTreeMap;
use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};
use svn_types::{ErrorCode, NodeKind, RelPath, Result, SvnError, safe_join, write_file_replacing};

use super::nodes::{Presence, Props};
use super::ops::BaseNode;
use super::{ADM_DIR, WcDb, sql_err};
use crate::conflicts::prej::prej_text;
use crate::conflicts::{Conflict, ConflictSkel, Operation};
use crate::pristine::{
    Md5Digest, PristineError, PristineIndex, PristineMode, PristineRecord, PristineStore,
    Sha1Digest,
};
use crate::skel::Skel;
use crate::translate::flags::SyncInput;
use crate::translate::utc_offset_zero;
use crate::workqueue::{
    FileInfoRecord, NodeInstallInfo, RunnerConfig, WorkQueueError, WorkQueueRunner, WorkQueueStore,
    WorkingCopyOps,
};

use super::node_api::FileInfo;

// ---- pristine index ------------------------------------------------------------------------

/// The `PRISTINE` table of a working copy as a [`PristineIndex`]. Each call is one statement in
/// its own transaction; database errors keep their classification (busy, corrupt, ...).
pub struct DbPristineIndex<'a> {
    db: &'a WcDb,
}

impl<'a> DbPristineIndex<'a> {
    /// Index over `db`.
    pub fn new(db: &'a WcDb) -> Self {
        Self { db }
    }
}

impl PristineIndex for DbPristineIndex<'_> {
    fn record(&mut self, r: &PristineRecord) -> std::result::Result<(), PristineError> {
        let size = i64::try_from(r.size).map_err(|_| PristineError::index("size out of range"))?;
        self.db.with_write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO pristine (checksum, compression, size, refcount, \
                 md5_checksum) VALUES (?1, NULL, ?2, 0, ?3)",
                params![r.sha1.to_svn_string(), size, r.md5.to_svn_string()],
            )
            .map_err(sql_err)?;
            Ok(())
        })?;
        Ok(())
    }

    fn lookup(
        &self,
        sha1: &Sha1Digest,
    ) -> std::result::Result<Option<PristineRecord>, PristineError> {
        let hydrated_col = if self.db.format() >= 32 {
            "hydrated"
        } else {
            "1"
        };
        let row: Option<(i64, String, i64)> = self.db.with_read(|c| {
            c.query_row(
                &format!(
                    "SELECT size, md5_checksum, {hydrated_col} FROM pristine WHERE checksum = ?1"
                ),
                [sha1.to_svn_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql_err)
        })?;
        row.map(|(size, md5, hydrated)| {
            Ok(PristineRecord {
                sha1: *sha1,
                md5: Md5Digest::from_svn_string(&md5)
                    .ok_or_else(|| PristineError::index("malformed md5 in PRISTINE"))?,
                size: u64::try_from(size)
                    .map_err(|_| PristineError::index("negative pristine size"))?,
                hydrated: hydrated != 0,
            })
        })
        .transpose()
    }

    fn unreferenced(&self) -> std::result::Result<Vec<Sha1Digest>, PristineError> {
        let keys: Vec<String> = self.db.with_read(|c| {
            let mut stmt = c
                .prepare("SELECT checksum FROM pristine WHERE refcount = 0")
                .map_err(sql_err)?;
            let rows = stmt.query_map([], |r| r.get(0)).map_err(sql_err)?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
                .map_err(sql_err)
        })?;
        Ok(keys
            .iter()
            .filter_map(|k| Sha1Digest::from_svn_string(k))
            .collect())
    }

    fn forget_if_unreferenced(
        &mut self,
        sha1: &Sha1Digest,
    ) -> std::result::Result<bool, PristineError> {
        let n = self.db.with_write(|tx| {
            tx.execute(
                "DELETE FROM pristine WHERE checksum = ?1 AND refcount = 0 \
                 AND NOT EXISTS (SELECT 1 FROM actual_node WHERE older_checksum = ?1 \
                 OR left_checksum = ?1 OR right_checksum = ?1)",
                [sha1.to_svn_string()],
            )
            .map_err(sql_err)
        })?;
        Ok(n > 0)
    }
}

// ---- work queue store ----------------------------------------------------------------------

/// The `WORK_QUEUE` table as a [`WorkQueueStore`], backed by
/// [`WcDb::wq_record_and_fetch_next`] (delete, stat cache and fetch in one transaction).
pub struct DbWorkQueue<'a> {
    db: &'a WcDb,
}

impl<'a> DbWorkQueue<'a> {
    /// Queue of `db`.
    pub fn new(db: &'a WcDb) -> Self {
        Self { db }
    }
}

fn rel(relpath: &str) -> std::result::Result<RelPath, WorkQueueError> {
    RelPath::new(relpath).map_err(|e| WorkQueueError::UnsafePath(format!("{relpath}: {e}")))
}

impl WorkQueueStore for DbWorkQueue<'_> {
    fn next_item(
        &mut self,
        completed: Option<u64>,
        fileinfo: &[FileInfoRecord],
    ) -> std::result::Result<Option<(u64, Vec<u8>)>, WorkQueueError> {
        let completed = completed
            .map(|id| {
                i64::try_from(id)
                    .map_err(|_| SvnError::new(ErrorCode::Internal, "work item id out of range"))
            })
            .transpose()?;
        let infos = fileinfo
            .iter()
            .map(|r| {
                Ok((
                    rel(&r.relpath)?,
                    FileInfo {
                        translated_size: Some(r.translated_size),
                        last_mod_time: Some(r.last_mod_time_micros),
                    },
                ))
            })
            .collect::<std::result::Result<Vec<_>, WorkQueueError>>()?;
        let next = self.db.wq_record_and_fetch_next_raw(completed, &infos)?;
        next.map(|(id, blob)| {
            u64::try_from(id)
                .map(|id| (id, blob))
                .map_err(|_| SvnError::new(ErrorCode::Corrupt, "negative work item id").into())
        })
        .transpose()
    }
}

// ---- working copy callbacks ----------------------------------------------------------------

/// Post-commit file fixup used by the `file-commit` work item; see
/// [`DbWorkingCopyOps::with_file_commit`].
pub type FileCommitFn<'a> = Box<dyn FnMut(&WcDb, &RelPath) -> Result<()> + 'a>;

/// The database callbacks of work items ([`WorkingCopyOps`]) over a [`WcDb`].
///
/// `file-commit` (`process_commit_file_install`) is left to the commit code through
/// [`with_file_commit`](Self::with_file_commit); without it the item fails with
/// [`ErrorCode::WcNeedsCleanup`] and stays queued.
pub struct DbWorkingCopyOps<'a> {
    db: &'a WcDb,
    file_commit: Option<FileCommitFn<'a>>,
}

impl<'a> DbWorkingCopyOps<'a> {
    /// Callbacks over `db` without a `file-commit` handler.
    pub fn new(db: &'a WcDb) -> Self {
        Self {
            db,
            file_commit: None,
        }
    }

    /// Installs the handler run for `file-commit` items. It must be idempotent.
    pub fn with_file_commit(mut self, f: impl FnMut(&WcDb, &RelPath) -> Result<()> + 'a) -> Self {
        self.file_commit = Some(Box::new(f));
        self
    }
}

fn props_map(p: Option<&Props>) -> BTreeMap<String, Vec<u8>> {
    p.map(|v| {
        v.iter()
            .filter_map(|(k, val)| Some((String::from_utf8(k.clone()).ok()?, val.clone())))
            .collect()
    })
    .unwrap_or_default()
}

fn not_versioned(relpath: &str) -> SvnError {
    SvnError::new(
        ErrorCode::PathNotFound,
        format!("'{relpath}' is not under version control"),
    )
}

impl WorkingCopyOps for DbWorkingCopyOps<'_> {
    fn install_info(&self, relpath: &str) -> std::result::Result<NodeInstallInfo, WorkQueueError> {
        let rp = rel(relpath)?;
        let node = self
            .db
            .read_node(&rp)?
            .ok_or_else(|| not_versioned(relpath))?;
        let repos = self.db.read_repos_info(&rp)?;
        let has_lock = self.db.repos_lock(&rp)?.is_some();
        Ok(NodeInstallInfo {
            checksum: node
                .checksum
                .as_deref()
                .and_then(Sha1Digest::from_svn_string),
            props: props_map(node.properties.as_ref()),
            changed_date_micros: node.changed_date,
            changed_rev: node.changed_rev.and_then(|r| i64::try_from(r).ok()),
            changed_author: node.changed_author,
            repos_relpath: repos.repos_relpath,
            repos_root_url: repos.repos_root_url,
            is_added: node.op_depth > 0 && node.presence == Presence::Normal,
            has_lock,
        })
    }

    fn actual_props(
        &self,
        relpath: &str,
    ) -> std::result::Result<BTreeMap<String, Vec<u8>>, WorkQueueError> {
        Ok(props_map(Some(&self.db.actual_props(&rel(relpath)?)?)))
    }

    fn sync_flags_input(
        &self,
        relpath: &str,
    ) -> std::result::Result<Option<SyncInput>, WorkQueueError> {
        let rp = rel(relpath)?;
        let Some(node) = self.db.read_node(&rp)? else {
            return Ok(None);
        };
        if node.kind != NodeKind::File || node.presence != Presence::Normal {
            return Ok(None);
        }
        let actual = props_map(Some(&self.db.actual_props(&rp)?));
        let pristine = props_map(node.properties.as_ref());
        Ok(Some(SyncInput {
            status_is_normal: node.op_depth == 0,
            needs_lock_working: actual.contains_key("svn:needs-lock"),
            needs_lock_pristine: pristine.contains_key("svn:needs-lock"),
            has_lock: self.db.repos_lock(&rp)?.is_some(),
            executable: actual.contains_key("svn:executable"),
        }))
    }

    fn write_prop_reject_file(
        &mut self,
        relpath: &str,
        legacy: Option<&Skel>,
    ) -> std::result::Result<(), WorkQueueError> {
        if legacy.is_some() {
            return Err(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                format!(
                    "'{relpath}' has an interrupted operation queued by svn 1.7 or 1.8; finish it \
                     with 'svn cleanup' using the client that created the working copy"
                ),
            )
            .into());
        }
        let rp = rel(relpath)?;
        let Some(record) = self.db.conflict(&rp)? else {
            return Ok(());
        };
        let skel = ConflictSkel::from_skel(&record.skel)?;
        let Some(prop) = skel.conflicts.iter().find_map(|c| match c {
            Conflict::Prop(p) => Some(p),
            _ => None,
        }) else {
            return Ok(());
        };
        let Some(marker) = prop.markers.first() else {
            return Ok(());
        };
        let operation = skel
            .operation
            .as_ref()
            .map_or(Operation::Update, |o| o.operation.clone());
        let pristine = self.db.pristine_props(&rp)?;
        let text = prej_text(&operation, prop, Some(pristine.as_slice()));
        let target = safe_join(self.db.wcroot(), &rel(marker)?)?;
        write_file_replacing(&target, &text)?;
        Ok(())
    }

    fn file_commit(&mut self, relpath: &str) -> std::result::Result<(), WorkQueueError> {
        let rp = rel(relpath)?;
        match self.file_commit.as_mut() {
            Some(f) => Ok(f(self.db, &rp)?),
            None => Err(SvnError::new(
                ErrorCode::WcNeedsCleanup,
                format!("'{relpath}' has an unfinished commit; finish it with 'svn cleanup' first"),
            )
            .into()),
        }
    }

    fn base_remove(
        &mut self,
        relpath: &str,
        not_present_rev: Option<i64>,
    ) -> std::result::Result<(), WorkQueueError> {
        let rp = rel(relpath)?;
        self.db.with_txn(|t| {
            let old = t.read_base(&rp)?;
            t.base_remove(&rp)?;
            let placeholder = match (not_present_rev, old) {
                (Some(rev), Some(old)) => {
                    match (old.repos_id, old.repos_relpath, u64::try_from(rev)) {
                        (Some(id), Some(path), Ok(rev)) => Some((id, old.kind, path, rev)),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some((repos_id, kind, repos_relpath, revision)) = placeholder {
                let (root, uuid) = t.repository(repos_id)?;
                t.base_add(
                    &rp,
                    &BaseNode {
                        kind,
                        presence: Presence::NotPresent,
                        repos_root_url: root,
                        repos_uuid: uuid,
                        repos_relpath,
                        revision,
                        changed_rev: None,
                        changed_date: None,
                        changed_author: None,
                        depth: None,
                        properties: None,
                        checksum: None,
                        symlink_target: None,
                        translated_size: None,
                        last_mod_time: None,
                        inherited_props: None,
                        dav_cache: None,
                    },
                )?;
            }
            Ok(())
        })?;
        Ok(())
    }

    fn post_upgrade(&mut self) -> std::result::Result<(), WorkQueueError> {
        Ok(())
    }
}

// ---- wiring ---------------------------------------------------------------------------------

impl WcDb {
    /// The pristine store of this working copy (read only for format 32).
    pub fn pristine_store(&self) -> PristineStore<DbPristineIndex<'_>> {
        let mode = if self.is_writable() {
            PristineMode::Writable
        } else {
            PristineMode::ReadOnly
        };
        PristineStore::open(
            self.wcroot().join(ADM_DIR),
            mode,
            DbPristineIndex::new(self),
        )
    }

    /// Runs every pending work item with [`DbWorkingCopyOps::new`] callbacks and returns how
    /// many ran. The caller holds the WC write lock. `cancelled` is polled between items; when
    /// it returns `true` the run stops with [`ErrorCode::Cancelled`] and the remaining items
    /// stay queued.
    pub fn run_work_queue(&self, cancelled: Option<&dyn Fn() -> bool>) -> Result<usize> {
        self.run_work_queue_with(&mut DbWorkingCopyOps::new(self), cancelled)
    }

    /// [`run_work_queue`](Self::run_work_queue) with caller-configured callbacks (for example a
    /// `file-commit` handler).
    pub fn run_work_queue_with(
        &self,
        ops: &mut DbWorkingCopyOps<'_>,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<usize> {
        let store = self.pristine_store();
        let config = RunnerConfig {
            wc_root: PathBuf::from(self.wcroot()),
            tz: &utc_offset_zero,
            cancelled,
        };
        WorkQueueRunner::new(config, ops, &store)
            .run(&mut DbWorkQueue::new(self))
            .map_err(SvnError::from)
    }
}
