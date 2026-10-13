//! Post-commit processing: folds a merged commit into the working copy in one transaction.
//!
//! Idempotent: it can run again after a crash at any point, including after the node rows
//! were already updated.

use std::fs::File;
use std::path::Path;

use rusqlite::params;
use svn_types::{ErrorCode, RelPath, Result, SvnError};
use svn_wc::WcDb;
use svn_wc::db::{CommitInfo, sql_err};
use svn_wc::pristine::{Md5Digest, PristineError, Sha1Digest};
use svn_wc::workqueue::WorkItem;

use super::collect::{ItemAction, ItemKind};
use super::journal::{JournalData, JournalItem, JournalMerge};
use crate::local::content::stat_cache;
use crate::local::paths::abs_of;
use crate::local::wc_adapters::{pristine_store, skels};

fn bad_digest(what: &str, item: &JournalItem) -> SvnError {
    SvnError::new(
        ErrorCode::Corrupt,
        format!("journal has a malformed {what} for '{}'", item.relpath),
    )
}

/// Installs the committed texts as pristines and updates the working copy rows of every
/// committed path. The caller holds the WC lock and runs the work queue afterwards.
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
        let info = commit_info(db, journal, merged, item, &rel)?;
        if item.kind == ItemKind::File && item.action != ItemAction::Delete {
            work.push(WorkItem::FileCommit {
                relpath: item.relpath.clone(),
            });
        }
        batch.push((rel, info));
    }
    db.commit_nodes(&batch, &skels(&work))?;
    if !journal.keep_locks {
        release_lock_rows(db, journal)?;
    }
    Ok(())
}

fn commit_info(
    db: &WcDb,
    journal: &JournalData,
    merged: &JournalMerge,
    item: &JournalItem,
    rel: &RelPath,
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
        // The stat cache says "working file equals pristine": only true when the file did not
        // change since its text was read for the upload.
        if let Some(read_stat) = text.stat
            && stat_cache(&abs_of(db, rel)) == Some(read_stat)
        {
            info.translated_size = Some(read_stat.0);
            info.last_mod_time = Some(read_stat.1);
        }
    }
    Ok(info)
}

fn install_pristines(db: &WcDb, journal: &JournalData, data_dir: &Path) -> Result<()> {
    let mut store = pristine_store(db);
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

/// Drops the cached lock rows of paths whose tokens were used (the server released them).
fn release_lock_rows(db: &WcDb, journal: &JournalData) -> Result<()> {
    let tokens: Vec<&str> = journal
        .items
        .iter()
        .filter_map(|i| i.lock_token.as_deref())
        .collect();
    if tokens.is_empty() {
        return Ok(());
    }
    db.with_write(|tx| {
        for t in tokens {
            tx.execute("DELETE FROM lock WHERE lock_token = ?1", params![t])
                .map_err(sql_err)?;
        }
        Ok(())
    })
}
