//! Post-commit processing: folds a merged commit into the working copy in one transaction.
//!
//! Idempotent: it can run again after a crash at any point, including after the node rows
//! were already updated.

use std::fs::File;
use std::path::Path;

use svn_types::{ErrorCode, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::db::CommitInfo;
use svn_wc::pristine::{Md5Digest, PristineError, Sha1Digest};
use svn_wc::workqueue::WorkItem;

use super::collect::{ItemAction, ItemKind};
use super::journal::{JournalData, JournalItem, JournalMerge};

fn bad_digest(what: &str, item: &JournalItem) -> SvnError {
    SvnError::new(
        ErrorCode::Corrupt,
        format!("journal has a malformed {what} for '{}'", item.relpath),
    )
}

/// Installs the committed texts as pristines and updates the working copy rows of every
/// committed path. The caller holds the WC lock and runs the work queue afterwards.
///
/// The node rows, the released lock rows and the `file-commit` work items are written in ONE
/// transaction (`svn_wc__db_global_commit` plus `svn_wc__db_lock_remove` with their work items):
/// after a crash either nothing changed (the journal finalizes again) or everything did and the
/// queued items finish the working files.
pub fn finalize(db: &WcDb, journal: &JournalData, data_dir: &Path) -> Result<()> {
    let merged = journal
        .merged
        .as_ref()
        .ok_or_else(|| SvnError::new(ErrorCode::Internal, "commit was not merged"))?;
    install_pristines(db, journal, data_dir)?;
    let mut items: Vec<&JournalItem> = journal.items.iter().collect();
    items.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    let mut batch = Vec::with_capacity(items.len());
    let mut work = Vec::new();
    for item in items {
        let rel = RelPath::new(&item.relpath)?;
        let info = commit_info(journal, merged, item)?;
        if item.kind == ItemKind::File && item.action != ItemAction::Delete {
            work.push(
                WorkItem::FileCommit {
                    relpath: item.relpath.clone(),
                }
                .to_skel(),
            );
        }
        batch.push((rel, info, item.lock_token.is_some()));
    }
    let release_locks = !journal.keep_locks;
    db.with_txn(|t| {
        for (rel, info, had_lock) in &batch {
            // The server released the lock with the commit; drop the cached row while BASE
            // still names the repository location it is keyed by.
            if release_locks && *had_lock {
                t.lock_remove(rel)?;
            }
            t.commit_node(rel, info)?;
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

fn install_pristines(db: &WcDb, journal: &JournalData, data_dir: &Path) -> Result<()> {
    let mut store = db.pristine_store();
    for item in &journal.items {
        let Some(text) = &item.text else { continue };
        let sha1 = Sha1Digest::from_hex(&text.sha1).ok_or_else(|| bad_digest("sha1", item))?;
        let md5 = Md5Digest::from_hex(&text.md5).ok_or_else(|| bad_digest("md5", item))?;
        let path = data_dir.join(&text.data);
        let mut file = match File::open(&path) {
            Ok(f) => f,
            // A re-run after the data was cleaned up: fine when the pristine is already there.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && svn_wc::pristine::PristineIndex::lookup(store.index(), &sha1)
                        .map_err(SvnError::from)?
                        .is_some() =>
            {
                continue;
            }
            Err(e) => {
                return Err(SvnError::from_io(
                    format!("open committed text '{}'", path.display()),
                    &e,
                ));
            }
        };
        store
            .install(&mut file, Some(&sha1), Some(&md5))
            .map_err(|e: PristineError| SvnError::from(e))?;
    }
    Ok(())
}
