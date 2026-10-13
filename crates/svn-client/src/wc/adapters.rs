//! Implementations of the `svn-wc` store traits over a [`WcDb`] handle: the pristine index, the
//! persistent work queue and the database callbacks of work items.
//!
//! These live here because `svn-wc` ships the traits and the runner but no `WcDb` binding yet;
//! they only use the public `with_read` / `with_write` access of the facade.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{OptionalExtension, params};
use svn_types::{CancelToken, ErrorCode, RelPath, Result, SvnError, safe_join};
use svn_wc::conflicts::{Conflict, ConflictSkel};
use svn_wc::db::{BaseNode, Presence, WcDb, sql_err};
use svn_wc::pristine::{
    Md5Digest, PristineError, PristineIndex, PristineMode, PristineRecord, PristineStore,
    Sha1Digest,
};
use svn_wc::skel::Skel;
use svn_wc::translate::flags::SyncInput;
use svn_wc::translate::utc_offset_zero;
use svn_wc::workqueue::{
    FileInfoRecord, NodeInstallInfo, RunnerConfig, WorkQueueError, WorkQueueRunner,
    WorkQueueStore, WorkingCopyOps,
};

/// Pristine table access for the store.
pub struct DbPristineIndex<'a>(pub &'a WcDb);

fn index_err(e: &SvnError) -> PristineError {
    PristineError::index(e.message.clone())
}

impl PristineIndex for DbPristineIndex<'_> {
    fn record(&mut self, r: &PristineRecord) -> std::result::Result<(), PristineError> {
        let size = i64::try_from(r.size).map_err(|_| PristineError::index("size out of range"))?;
        self.0
            .with_write(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO pristine (checksum, compression, size, refcount, \
                     md5_checksum) VALUES (?1, NULL, ?2, 0, ?3)",
                    params![r.sha1.to_svn_string(), size, r.md5.to_svn_string()],
                )
                .map_err(sql_err)?;
                Ok(())
            })
            .map_err(|e| index_err(&e))
    }

    fn lookup(
        &self,
        sha1: &Sha1Digest,
    ) -> std::result::Result<Option<PristineRecord>, PristineError> {
        let hydrated_col = if self.0.format() >= 32 { "hydrated" } else { "1" };
        self.0
            .with_read(|c| {
                let row: Option<(i64, String, i64)> = c
                    .query_row(
                        &format!(
                            "SELECT size, md5_checksum, {hydrated_col} FROM pristine \
                             WHERE checksum = ?1"
                        ),
                        [sha1.to_svn_string()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(sql_err)?;
                Ok(row)
            })
            .map_err(|e| index_err(&e))?
            .map(|(size, md5, hydrated)| {
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
        self.0
            .with_read(|c| {
                let mut stmt = c
                    .prepare("SELECT checksum FROM pristine WHERE refcount = 0")
                    .map_err(sql_err)?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(sql_err)?;
                let mut out = Vec::new();
                for row in rows {
                    if let Some(d) = Sha1Digest::from_svn_string(&row.map_err(sql_err)?) {
                        out.push(d);
                    }
                }
                Ok(out)
            })
            .map_err(|e| index_err(&e))
    }

    fn forget_if_unreferenced(
        &mut self,
        sha1: &Sha1Digest,
    ) -> std::result::Result<bool, PristineError> {
        self.0
            .with_write(|tx| {
                let key = sha1.to_svn_string();
                let n = tx
                    .execute(
                        "DELETE FROM pristine WHERE checksum = ?1 AND refcount = 0 \
                         AND NOT EXISTS (SELECT 1 FROM actual_node WHERE older_checksum = ?1 \
                         OR left_checksum = ?1 OR right_checksum = ?1)",
                        [key],
                    )
                    .map_err(sql_err)?;
                Ok(n > 0)
            })
            .map_err(|e| index_err(&e))
    }
}

/// Opens the pristine store of `db` (read only for format 32).
pub fn pristine_store(db: &WcDb) -> PristineStore<DbPristineIndex<'_>> {
    let mode = if db.is_writable() {
        PristineMode::Writable
    } else {
        PristineMode::ReadOnly
    };
    PristineStore::open(db.wcroot().join(svn_wc::db::ADM_DIR), mode, DbPristineIndex(db))
}

/// The persistent work queue of a working copy.
pub struct DbQueue<'a>(pub &'a WcDb);

impl WorkQueueStore for DbQueue<'_> {
    fn next_item(
        &mut self,
        completed: Option<u64>,
        fileinfo: &[FileInfoRecord],
    ) -> std::result::Result<Option<(u64, Vec<u8>)>, WorkQueueError> {
        let next = self.0.with_write(|tx| {
            if let Some(id) = completed {
                let id = i64::try_from(id)
                    .map_err(|_| SvnError::new(ErrorCode::Internal, "work item id"))?;
                tx.execute("DELETE FROM work_queue WHERE id = ?1", [id])
                    .map_err(sql_err)?;
            }
            for rec in fileinfo {
                // A node removed by an earlier item of the same run has nothing to record.
                tx.execute(
                    "UPDATE nodes SET translated_size = ?3, last_mod_time = ?4 \
                     WHERE wc_id = ?1 AND local_relpath = ?2 AND op_depth = \
                     (SELECT MAX(op_depth) FROM nodes WHERE wc_id = ?1 AND local_relpath = ?2)",
                    params![
                        self.0.wc_id(),
                        rec.relpath,
                        rec.translated_size,
                        rec.last_mod_time_micros
                    ],
                )
                .map_err(sql_err)?;
            }
            tx.query_row(
                "SELECT id, work FROM work_queue ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(sql_err)
        })?;
        Ok(next.map(|(id, blob)| (u64::try_from(id).unwrap_or(0), blob)))
    }
}

fn rel(relpath: &str) -> std::result::Result<RelPath, WorkQueueError> {
    RelPath::new(relpath).map_err(|e| WorkQueueError::UnsafePath(format!("{relpath}: {e}")))
}

fn to_sha1(checksum: Option<&str>) -> Option<Sha1Digest> {
    checksum.and_then(Sha1Digest::from_svn_string)
}

fn props_map(p: Option<&Vec<(Vec<u8>, Vec<u8>)>>) -> BTreeMap<String, Vec<u8>> {
    p.map(|v| {
        v.iter()
            .filter_map(|(k, val)| Some((String::from_utf8(k.clone()).ok()?, val.clone())))
            .collect()
    })
    .unwrap_or_default()
}

/// Database callbacks of work items.
pub struct DbOps<'a>(pub &'a WcDb);

impl WorkingCopyOps for DbOps<'_> {
    fn install_info(&self, relpath: &str) -> std::result::Result<NodeInstallInfo, WorkQueueError> {
        let rp = rel(relpath)?;
        let node = self.0.read_node(&rp)?.ok_or_else(|| {
            SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{relpath}' is not under version control"),
            )
        })?;
        let root: Option<String> = self.0.with_read(|c| match node.repos_id {
            Some(id) => c
                .query_row("SELECT root FROM repository WHERE id = ?1", [id], |r| r.get(0))
                .optional()
                .map_err(sql_err),
            None => c
                .query_row("SELECT root FROM repository LIMIT 1", [], |r| r.get(0))
                .optional()
                .map_err(sql_err),
        })?;
        let has_lock = self.0.repos_lock(&rp)?.is_some();
        Ok(NodeInstallInfo {
            checksum: to_sha1(node.checksum.as_deref()),
            props: props_map(node.properties.as_ref()),
            changed_date_micros: node.changed_date,
            changed_rev: node.changed_rev.and_then(|r| i64::try_from(r).ok()),
            changed_author: node.changed_author,
            repos_relpath: node.repos_relpath.unwrap_or_default(),
            repos_root_url: root.unwrap_or_default(),
            is_added: node.op_depth > 0 && node.presence == Presence::Normal,
            has_lock,
        })
    }

    fn actual_props(
        &self,
        relpath: &str,
    ) -> std::result::Result<BTreeMap<String, Vec<u8>>, WorkQueueError> {
        let p = self.0.actual_props(&rel(relpath)?)?;
        Ok(props_map(Some(&p)))
    }

    fn sync_flags_input(
        &self,
        relpath: &str,
    ) -> std::result::Result<Option<SyncInput>, WorkQueueError> {
        let rp = rel(relpath)?;
        let Some(node) = self.0.read_node(&rp)? else {
            return Ok(None);
        };
        if node.kind != svn_types::NodeKind::File || node.presence != Presence::Normal {
            return Ok(None);
        }
        let actual = props_map(Some(&self.0.actual_props(&rp)?));
        let pristine = props_map(node.properties.as_ref());
        Ok(Some(SyncInput {
            status_is_normal: node.op_depth == 0,
            needs_lock_working: actual.contains_key("svn:needs-lock"),
            needs_lock_pristine: pristine.contains_key("svn:needs-lock"),
            has_lock: self.0.repos_lock(&rp)?.is_some(),
            executable: actual.contains_key("svn:executable"),
        }))
    }

    fn write_prop_reject_file(
        &mut self,
        relpath: &str,
        _legacy: Option<&Skel>,
    ) -> std::result::Result<(), WorkQueueError> {
        let rp = rel(relpath)?;
        let Some(record) = self.0.conflict(&rp)? else {
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
        let target = safe_join(self.0.wcroot(), &rel(marker)?)?;
        let text = reject_text(prop);
        std::fs::write(&target, text).map_err(|e| SvnError::from_io(format!("cannot write {}", target.display()), &e))?;
        Ok(())
    }

    fn file_commit(&mut self, relpath: &str) -> std::result::Result<(), WorkQueueError> {
        Err(SvnError::new(
            ErrorCode::WcNeedsCleanup,
            format!(
                "'{relpath}' has an unfinished commit; finish it with 'svn cleanup' before updating"
            ),
        )
        .into())
    }

    fn base_remove(
        &mut self,
        relpath: &str,
        not_present_rev: Option<i64>,
    ) -> std::result::Result<(), WorkQueueError> {
        let rp = rel(relpath)?;
        let old = self.0.read_base(&rp)?;
        self.0.base_remove(&rp, &[])?;
        if let (Some(rev), Some(old)) = (not_present_rev, old)
            && let (Some(path), Ok(rev)) = (old.repos_relpath.clone(), u64::try_from(rev))
        {
            let (root, uuid) = super::repos_identity(self.0)?;
            self.0.base_add(
                &rp,
                &BaseNode {
                    kind: old.kind,
                    presence: Presence::NotPresent,
                    repos_root_url: root,
                    repos_uuid: uuid,
                    repos_relpath: path,
                    revision: rev,
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
                &[],
            )?;
        }
        Ok(())
    }

    fn post_upgrade(&mut self) -> std::result::Result<(), WorkQueueError> {
        Ok(())
    }
}

/// Text of a `.prej` file for a property conflict, byte-compatible with what svn 1.15 writes.
fn reject_text(p: &svn_wc::conflicts::PropConflict) -> String {
    let find = |set: &Option<Vec<(Vec<u8>, Vec<u8>)>>, name: &str| {
        set.as_ref().and_then(|s| {
            s.iter()
                .find(|(k, _)| k == name.as_bytes())
                .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        })
    };
    let mut out = String::new();
    for name in &p.names {
        let old = find(&p.old_props, name);
        let mine = find(&p.mine_props, name);
        let theirs = find(&p.their_props, name);
        let (verb, why) = match (&old, &theirs, &mine) {
            (_, None, _) => ("delete property", "has been locally modified"),
            (None, Some(_), _) => ("add new property", "already exists"),
            (Some(_), Some(_), None) => ("change property", "has been locally deleted"),
            (Some(_), Some(_), Some(_)) => (
                "change property",
                "has already been locally changed to a different value",
            ),
        };
        out.push_str(&format!(
            "Trying to {verb} '{name}'\nbut the property {why}.\n<<<<<<< (local property value)\n"
        ));
        out.push_str(mine.as_deref().unwrap_or(""));
        out.push_str("||||||| (incoming 'changed from' value)\n");
        out.push_str(old.as_deref().unwrap_or(""));
        out.push_str("=======\n");
        out.push_str(theirs.as_deref().unwrap_or(""));
        out.push_str(">>>>>>> (incoming 'changed to' value)\n");
    }
    out
}

/// Runs every pending work item. Fails with [`ErrorCode::Cancelled`] between items when `cancel`
/// fires; the unfinished items stay queued.
pub fn run_work_queue(db: &WcDb, cancel: &CancelToken) -> Result<usize> {
    let store = pristine_store(db);
    let mut ops = DbOps(db);
    let mut queue = DbQueue(db);
    let is_cancelled = || cancel.is_cancelled();
    let config = RunnerConfig {
        wc_root: Path::new(db.wcroot()).to_path_buf(),
        tz: &utc_offset_zero,
        cancelled: Some(&is_cancelled),
    };
    WorkQueueRunner::new(config, &mut ops, &store)
        .run(&mut queue)
        .map_err(SvnError::from)
}
