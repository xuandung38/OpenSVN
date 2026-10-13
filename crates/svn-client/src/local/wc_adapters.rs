//! Adapters that let the `svn-wc` work queue runner and pristine store operate on a [`WcDb`].
//!
//! `svn-wc` defines the runner against small traits so it stays testable without a database;
//! this module implements them over the public `WcDb` API. Every method is idempotent, as the
//! work queue contract requires (a crash can replay an item whose effect already happened).
//!
//! `$Date$` keywords are expanded in UTC: this crate has no time-zone database.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};
use svn_types::{CancelToken, ErrorCode, NodeKind, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::db::{Node, Presence, sql_err};
use svn_wc::pristine::{
    Md5Digest, PristineError, PristineIndex, PristineMode, PristineRecord, PristineStore,
    Sha1Digest,
};
use svn_wc::skel::Skel;
use svn_wc::translate::flags::SyncInput;
use svn_wc::translate::{self, KeywordContext, TranslateInfo, utc_offset_zero};
use svn_wc::workqueue::{
    FileInfoRecord, NodeInstallInfo, RunnerConfig, WorkItem, WorkQueueError, WorkQueueRunner,
    WorkQueueStore, WorkingCopyOps,
};

use super::paths::abs_of;

/// `PRISTINE` table access for [`PristineStore`].
pub struct WcdbPristineIndex<'a> {
    db: &'a WcDb,
}

fn idx_err(e: SvnError) -> PristineError {
    PristineError::index(e.message)
}

impl PristineIndex for WcdbPristineIndex<'_> {
    fn record(&mut self, r: &PristineRecord) -> std::result::Result<(), PristineError> {
        self.db
            .with_write(|tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO pristine (checksum, compression, size, refcount, \
                     md5_checksum) VALUES (?1, NULL, ?2, 0, ?3)",
                    params![
                        r.sha1.to_svn_string(),
                        i64::try_from(r.size).unwrap_or(i64::MAX),
                        r.md5.to_svn_string()
                    ],
                )
                .map_err(sql_err)?;
                Ok(())
            })
            .map_err(idx_err)
    }

    fn lookup(
        &self,
        sha1: &Sha1Digest,
    ) -> std::result::Result<Option<PristineRecord>, PristineError> {
        self.db
            .with_read(|c| {
                let row: Option<(i64, String)> = c
                    .query_row(
                        "SELECT size, md5_checksum FROM pristine WHERE checksum = ?1",
                        [sha1.to_svn_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(sql_err)?;
                Ok(row.and_then(|(size, md5)| {
                    Some(PristineRecord {
                        sha1: *sha1,
                        md5: Md5Digest::from_svn_string(&md5)?,
                        size: u64::try_from(size).ok()?,
                        hydrated: true,
                    })
                }))
            })
            .map_err(idx_err)
    }

    fn unreferenced(&self) -> std::result::Result<Vec<Sha1Digest>, PristineError> {
        self.db
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
            .map_err(idx_err)
    }

    fn forget_if_unreferenced(
        &mut self,
        sha1: &Sha1Digest,
    ) -> std::result::Result<bool, PristineError> {
        self.db
            .with_write(|tx| {
                let n = tx
                    .execute(
                        "DELETE FROM pristine WHERE checksum = ?1 AND refcount = 0 \
                         AND NOT EXISTS (SELECT 1 FROM actual_node WHERE older_checksum = ?1 \
                         OR left_checksum = ?1 OR right_checksum = ?1)",
                        [sha1.to_svn_string()],
                    )
                    .map_err(sql_err)?;
                Ok(n > 0)
            })
            .map_err(idx_err)
    }
}

/// Opens the pristine store of `db` (read-only for format 32).
pub fn pristine_store(db: &WcDb) -> PristineStore<WcdbPristineIndex<'_>> {
    let mode = if db.is_writable() {
        PristineMode::Writable
    } else {
        PristineMode::ReadOnly
    };
    PristineStore::open(
        db.wcroot().join(svn_wc::db::ADM_DIR),
        mode,
        WcdbPristineIndex { db },
    )
}

fn wq_svn(e: SvnError) -> WorkQueueError {
    WorkQueueError::Svn(e)
}

fn rel(relpath: &str) -> std::result::Result<RelPath, WorkQueueError> {
    RelPath::new(relpath).map_err(wq_svn)
}

fn props_map(props: Option<&Vec<(Vec<u8>, Vec<u8>)>>) -> BTreeMap<String, Vec<u8>> {
    props
        .map(|p| {
            p.iter()
                .map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Repository location `(root url, uuid, relpath)` where `path` lives or will live once
/// committed (for copies the destination, not the copy source).
pub fn repos_location(db: &WcDb, path: &RelPath) -> Result<(String, String, String)> {
    let relpath = crate::commit::collect::destination_repos_relpath(db, path)?;
    let (root, uuid) = db.with_read(|c| {
        c.query_row(
            "SELECT r.root, r.uuid FROM repository r JOIN nodes n ON n.repos_id = r.id \
             WHERE n.local_relpath = '' AND n.op_depth = 0",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .map_err(sql_err)
    })?;
    Ok((root, uuid, relpath))
}

/// Work queue store and node operations backed by a [`WcDb`].
pub struct WcAdapter<'a> {
    db: &'a WcDb,
}

impl<'a> WcAdapter<'a> {
    /// Wraps `db`.
    pub fn new(db: &'a WcDb) -> Self {
        Self { db }
    }
}

impl WorkQueueStore for WcAdapter<'_> {
    fn next_item(
        &mut self,
        completed: Option<u64>,
        fileinfo: &[FileInfoRecord],
    ) -> std::result::Result<Option<(u64, Vec<u8>)>, WorkQueueError> {
        // Order matters for crash replay: stat cache first (harmless to repeat), then the
        // item is deleted.
        for rec in fileinfo {
            let path = rel(&rec.relpath)?;
            self.db
                .record_fileinfo(
                    &path,
                    Some(rec.translated_size),
                    Some(rec.last_mod_time_micros),
                )
                .or_else(|e| {
                    // The node may have vanished since the item ran; that is not an error.
                    if e.code == ErrorCode::PathNotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .map_err(wq_svn)?;
        }
        if let Some(id) = completed {
            let id = i64::try_from(id)
                .map_err(|_| wq_svn(SvnError::new(ErrorCode::Internal, "work id out of range")))?;
            self.db.complete_work_item(id).map_err(wq_svn)?;
        }
        let next = self
            .db
            .with_read(|c| {
                c.query_row(
                    "SELECT id, work FROM work_queue ORDER BY id LIMIT 1",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
                )
                .optional()
                .map_err(sql_err)
            })
            .map_err(wq_svn)?;
        Ok(next.and_then(|(id, work)| Some((u64::try_from(id).ok()?, work))))
    }
}

impl WcAdapter<'_> {
    fn node(&self, relpath: &str) -> std::result::Result<(RelPath, Option<Node>), WorkQueueError> {
        let p = rel(relpath)?;
        let n = self.db.read_node(&p).map_err(wq_svn)?;
        Ok((p, n))
    }

    /// Working file content in repo-normal form equals the node's pristine text.
    fn working_matches_pristine(
        &self,
        path: &RelPath,
        node: &Node,
    ) -> std::result::Result<bool, WorkQueueError> {
        let Some(expected) = node
            .checksum
            .as_deref()
            .and_then(Sha1Digest::from_svn_string)
        else {
            return Ok(false);
        };
        let abs = abs_of(self.db, path);
        match super::content::detranslate_digest(self.db, path, &abs) {
            Ok(d) => Ok(d.sha1 == expected),
            Err(e) if e.code == ErrorCode::PathNotFound => Ok(false),
            Err(e) => Err(wq_svn(e)),
        }
    }
}

impl WorkingCopyOps for WcAdapter<'_> {
    fn install_info(&self, relpath: &str) -> std::result::Result<NodeInstallInfo, WorkQueueError> {
        let (path, node) = self.node(relpath)?;
        let node = node.ok_or_else(|| {
            wq_svn(SvnError::new(
                ErrorCode::PathNotFound,
                format!("'{relpath}' is not under version control"),
            ))
        })?;
        let (root, _uuid, repos_relpath) = repos_location(self.db, &path).map_err(wq_svn)?;
        Ok(NodeInstallInfo {
            checksum: node
                .checksum
                .as_deref()
                .and_then(Sha1Digest::from_svn_string),
            props: props_map(node.properties.as_ref()),
            changed_date_micros: node.changed_date,
            changed_rev: node.changed_rev.and_then(|r| i64::try_from(r).ok()),
            changed_author: node.changed_author.clone(),
            repos_relpath,
            repos_root_url: root,
            is_added: node.op_depth > 0,
            has_lock: self.db.repos_lock(&path).map_err(wq_svn)?.is_some(),
        })
    }

    fn actual_props(
        &self,
        relpath: &str,
    ) -> std::result::Result<BTreeMap<String, Vec<u8>>, WorkQueueError> {
        let p = rel(relpath)?;
        let props = self.db.actual_props(&p).map_err(wq_svn)?;
        Ok(props_map(Some(&props)))
    }

    fn sync_flags_input(
        &self,
        relpath: &str,
    ) -> std::result::Result<Option<SyncInput>, WorkQueueError> {
        let (path, node) = self.node(relpath)?;
        let Some(node) = node else { return Ok(None) };
        if node.kind != NodeKind::File || node.presence != Presence::Normal {
            return Ok(None);
        }
        let working = props_map(Some(&self.db.actual_props(&path).map_err(wq_svn)?));
        let pristine = props_map(node.properties.as_ref());
        Ok(Some(SyncInput {
            status_is_normal: node.op_depth == 0,
            needs_lock_working: working.contains_key("svn:needs-lock"),
            needs_lock_pristine: pristine.contains_key("svn:needs-lock"),
            has_lock: self.db.repos_lock(&path).map_err(wq_svn)?.is_some(),
            executable: working.contains_key("svn:executable"),
        }))
    }

    fn write_prop_reject_file(
        &mut self,
        relpath: &str,
        _legacy: Option<&Skel>,
    ) -> std::result::Result<(), WorkQueueError> {
        Err(wq_svn(SvnError::new(
            ErrorCode::WcNeedsCleanup,
            format!(
                "cannot write the property reject file of '{relpath}': not supported yet, \
                 run 'svn cleanup' with the client that queued it"
            ),
        )))
    }

    /// Post-commit fixups: re-expands keywords when the working file still equals the committed
    /// text, syncs read-only/executable bits and refreshes the stat cache.
    fn file_commit(&mut self, relpath: &str) -> std::result::Result<(), WorkQueueError> {
        let (path, node) = self.node(relpath)?;
        let Some(node) = node else { return Ok(()) };
        let abs = abs_of(self.db, &path);
        if node.kind != NodeKind::File || !abs.is_file() {
            return Ok(());
        }
        let props = props_map(Some(&self.db.actual_props(&path).map_err(wq_svn)?));
        let tinfo = TranslateInfo::from_props(|n| props.get(n).map(Vec::as_slice));
        let has_keywords = tinfo.keywords_prop.is_some() && !tinfo.special;
        if has_keywords && self.working_matches_pristine(&path, &node)? {
            let info = self.install_info(relpath)?;
            let ctx = KeywordContext {
                rev: Some(info.changed_rev.unwrap_or(-1)),
                url: Some(format!("{}/{}", info.repos_root_url, info.repos_relpath)),
                repos_root_url: Some(info.repos_root_url.clone()),
                date_micros: info.changed_date_micros,
                author: info.changed_author.clone(),
            };
            let pristine_hash = node
                .checksum
                .as_deref()
                .and_then(Sha1Digest::from_svn_string);
            if let Some(hash) = pristine_hash {
                let store = pristine_store(self.db);
                let pristine_path = store.path_of(&hash);
                let text = fs::read(&pristine_path).map_err(|source| WorkQueueError::Io {
                    op: "read",
                    path: pristine_path.clone(),
                    source,
                })?;
                let expanded = translate::to_working(&tinfo, &text, &ctx, &utc_offset_zero)?;
                svn_types::write_file_replacing(&abs, &expanded).map_err(wq_svn)?;
            }
        }
        if let Some(input) = self.sync_flags_input(relpath)? {
            translate::flags::sync_with_props(&abs, input)?;
        }
        if let Ok(md) = fs::metadata(&abs) {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_micros()).ok());
            self.db
                .record_fileinfo(&path, i64::try_from(md.len()).ok(), mtime)
                .map_err(wq_svn)?;
        }
        Ok(())
    }

    fn base_remove(
        &mut self,
        relpath: &str,
        _not_present_rev: Option<i64>,
    ) -> std::result::Result<(), WorkQueueError> {
        let p = rel(relpath)?;
        self.db.base_remove(&p, &[]).map_err(wq_svn)
    }

    fn post_upgrade(&mut self) -> std::result::Result<(), WorkQueueError> {
        Ok(())
    }
}

/// Runs the pending work queue of `db` to completion and returns how many items ran. The caller
/// must hold the WC lock. `cancel` is polled between items.
pub fn run_work_queue(db: &WcDb, cancel: &CancelToken) -> Result<usize> {
    let mut store = WcAdapter::new(db);
    let mut ops = WcAdapter::new(db);
    let pristine = pristine_store(db);
    let cancelled = || cancel.is_cancelled();
    let config = RunnerConfig {
        wc_root: PathBuf::from(db.wcroot()),
        tz: &utc_offset_zero,
        cancelled: Some(&cancelled),
    };
    let mut runner = WorkQueueRunner::new(config, &mut ops, &pristine);
    runner.run(&mut store).map_err(SvnError::from)
}

/// Serializes `items` for `WcDb` write methods.
pub fn skels(items: &[WorkItem]) -> Vec<Skel> {
    items.iter().map(WorkItem::to_skel).collect()
}
