//! Post-commit processing: folds a merged commit into the working copy in one transaction.
//!
//! Idempotent: it can run again after a crash at any point, including after the node rows
//! were already updated.

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;

use svn_types::{ErrorCode, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::db::CommitInfo;
use svn_wc::pristine::{Md5Digest, PristineError, PristineRecord, Sha1Digest};
use svn_wc::workqueue::WorkItem;

use super::collect::{ItemAction, ItemKind};
use super::journal::{JournalData, JournalItem, JournalMerge};
use super::parallel;

fn bad_digest(what: &str, item: &JournalItem) -> SvnError {
    SvnError::new(
        ErrorCode::Corrupt,
        format!("journal has a malformed {what} for '{}'", item.relpath),
    )
}

/// Installs the committed texts as pristines and updates the working copy rows of every
/// committed path. The caller holds the WC lock and runs the work queue afterwards.
///
/// The texts are synced and in place first (several at a time); their pristine rows, the node
/// rows, the released lock rows and the `file-commit` work items are then written in ONE
/// transaction (`svn_wc__db_global_commit` plus `svn_wc__db_lock_remove` with their work items):
/// after a crash either nothing changed (the journal finalizes again) or everything did and the
/// queued items finish the working files.
pub fn finalize(db: &WcDb, journal: &JournalData, data_dir: &Path) -> Result<()> {
    finalize_with(db, journal, data_dir, true)
}

/// [`finalize`] right after this process saved the committed texts (prepared, synced and
/// hashed them moments ago): the copies become pristines after a size check, without being
/// read and hashed again. Recovery uses [`finalize`], which verifies them.
pub fn finalize_fresh(db: &WcDb, journal: &JournalData, data_dir: &Path) -> Result<()> {
    finalize_with(db, journal, data_dir, false)
}

fn finalize_with(db: &WcDb, journal: &JournalData, data_dir: &Path, verify: bool) -> Result<()> {
    let merged = journal
        .merged
        .as_ref()
        .ok_or_else(|| SvnError::new(ErrorCode::Internal, "commit was not merged"))?;
    let pristines = install_pristines(db, journal, data_dir, verify)?;
    let mut items: Vec<&JournalItem> = journal.items.iter().collect();
    items.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    let mut batch = Vec::with_capacity(items.len());
    for item in items {
        let rel = RelPath::new(&item.relpath)?;
        let info = commit_info(journal, merged, item)?;
        let file_commit =
            (item.kind == ItemKind::File && item.action != ItemAction::Delete).then(|| {
                WorkItem::FileCommit {
                    relpath: item.relpath.clone(),
                }
                .to_skel()
            });
        batch.push((rel, info, file_commit));
    }
    let locks = if journal.keep_locks {
        Vec::new()
    } else {
        journal
            .locks
            .iter()
            .map(|l| Ok((RelPath::new(&l.relpath)?, l.token.as_str())))
            .collect::<Result<Vec<_>>>()?
    };
    db.with_txn(|t| {
        for r in &pristines {
            t.record_pristine(r)?;
        }
        // The server released the locks with the commit; drop the cached rows (only those
        // still holding the sent token) while BASE still names the repository locations they
        // are keyed by, including the nodes below a committed deletion.
        for (rel, token) in &locks {
            t.lock_remove_token(rel, token)?;
        }
        let mut work = Vec::new();
        for (rel, info, file_commit) in &batch {
            // A journal finalized late (recovery after the working copy moved on, e.g. the
            // same path committed again as a later revision) must never take BASE back.
            if t.read_base(rel)?
                .and_then(|b| b.revision)
                .is_some_and(|r| r > merged.revision)
            {
                continue;
            }
            t.commit_node(rel, info)?;
            work.extend(file_commit.iter().cloned());
        }
        t.queue_work(&work)
    })
}

fn commit_info(
    journal: &JournalData,
    merged: &JournalMerge,
    item: &JournalItem,
) -> Result<CommitInfo> {
    let mut info = CommitInfo {
        new_rev: merged.revision,
        date: merged.date,
        author: merged.author.clone(),
        keep_changelist: journal.keep_changelist,
        ..CommitInfo::default()
    };
    if let Some(text) = &item.text {
        let sha1 = Sha1Digest::from_hex(&text.sha1).ok_or_else(|| bad_digest("sha1", item))?;
        info.checksum = Some(sha1.to_svn_string());
    }
    // No stat cache, like `svn_wc__db_global_commit`: the `file-commit` item records it once
    // it has checked that the working file is what was committed.
    Ok(info)
}

/// Puts every committed text into the pristine store (synced, in place, read-only) and returns
/// the rows to record. A text whose row already exists is skipped: the row is authoritative,
/// and on a re-run after a crash the journal's copy may already be gone.
fn install_pristines(
    db: &WcDb,
    journal: &JournalData,
    data_dir: &Path,
    verify: bool,
) -> Result<Vec<PristineRecord>> {
    let store = db.pristine_store();
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for item in &journal.items {
        let Some(text) = &item.text else { continue };
        let sha1 = Sha1Digest::from_hex(&text.sha1).ok_or_else(|| bad_digest("sha1", item))?;
        let md5 = Md5Digest::from_hex(&text.md5).ok_or_else(|| bad_digest("md5", item))?;
        if !seen.insert(sha1) {
            continue;
        }
        match svn_wc::pristine::PristineIndex::lookup(store.index(), &sha1)
            .map_err(SvnError::from)?
        {
            Some(row) if row.hydrated => continue,
            Some(_) => return Err(PristineError::RehydrateUnsupported(sha1).into()),
            None => {}
        }
        let record = PristineRecord {
            sha1,
            md5,
            size: text.size,
            hydrated: true,
        };
        jobs.push((data_dir.join(&text.data), record));
    }
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let batch = store.batch().map_err(SvnError::from)?;
    // The journal's copy becomes the pristine (renamed, not copied, on the same file system).
    // Until the rows below are committed the journal stays; if this runs again after a crash
    // the copy may already be in the store, where it is checked and taken as it is.
    let records = parallel::map(jobs, |(path, record)| {
        let (sha1, md5) = (record.sha1, record.md5);
        let adopted = if verify {
            batch.adopt(&path, &sha1, &md5)
        } else {
            batch.adopt_trusted(&path, &record)
        };
        match adopted {
            Ok(r) => Ok(r),
            Err(PristineError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                let placed = batch.path_of(&sha1);
                let mut file = File::open(&placed).map_err(|e| {
                    SvnError::from_io(format!("open committed text '{}'", path.display()), &e)
                })?;
                batch
                    .stage(&mut file, Some(&sha1), Some(&md5))
                    .map_err(SvnError::from)
            }
            Err(e) => Err(e.into()),
        }
    })?;
    batch.sync_dirs(&records).map_err(SvnError::from)?;
    Ok(records)
}
