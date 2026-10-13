//! Crash recovery for commits: finishes or cleans up every journal left in the state directory.
//!
//! Call it when the application starts, before taking new write locks. A journal that records
//! a revision is finalized against its working copy; one that only records a server
//! transaction gets that transaction deleted; one that recorded neither is dropped.

use std::path::Path;

use svn_ra_api::RaSession;
use svn_types::{CancelToken, Result, SvnError};
use svn_wc::WcDb;

use super::finalize::finalize;
use super::journal::{JournalData, JournalStore};
use crate::local::lock::with_wc_lock;
use crate::wc::run_work_queue;

/// What [`recover_commits`] did.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Journals whose commit had been merged and was folded into the working copy.
    pub finalized: Vec<String>,
    /// Journals whose server transaction was deleted (or that never reached the server).
    pub aborted: Vec<String>,
    /// Journals that could not be processed; they are left in place for the next attempt.
    pub failed: Vec<(String, SvnError)>,
}

/// Processes all journals in `state_dir`. `open_wc` opens a working copy by root path with its
/// lock owner registered (see `WcDb::open_and_recover`).
pub async fn recover_commits(
    state_dir: &Path,
    ra: &dyn RaSession,
    cancel: &CancelToken,
    open_wc: &(dyn Fn(&Path) -> Result<WcDb> + Sync),
) -> Result<RecoveryReport> {
    let store = JournalStore::new(state_dir);
    let mut report = RecoveryReport::default();
    let mut authenticated = false;
    for path in store.list()? {
        let data = match JournalStore::read(&path) {
            Ok(d) => d,
            Err(e) => {
                let id = path.display().to_string();
                report.failed.push((id, e));
                continue;
            }
        };
        let id = data.id.clone();
        let outcome = if data.merged.is_some() {
            finalize_journal(&store, &data, cancel, open_wc).map(|()| true)
        } else {
            abort_journal(&store, &data, ra, cancel, &mut authenticated)
                .await
                .map(|()| false)
        };
        match outcome {
            Ok(true) => report.finalized.push(id),
            Ok(false) => report.aborted.push(id),
            Err(e) => report.failed.push((id, e)),
        }
    }
    Ok(report)
}

fn finalize_journal(
    store: &JournalStore,
    data: &JournalData,
    cancel: &CancelToken,
    open_wc: &(dyn Fn(&Path) -> Result<WcDb> + Sync),
) -> Result<()> {
    let db = open_wc(&data.wcroot)?;
    let data_dir = store.data_dir_of(&data.id);
    with_wc_lock(&db, cancel, || {
        finalize(&db, data, &data_dir)?;
        run_work_queue(&db, cancel)?;
        Ok(())
    })?;
    store.remove(&data.id)
}

/// Finalizes every merged journal of the working copy `db` (whose lock the caller holds), so
/// a commit never starts from a BASE that misses an earlier merged revision. Journals of other
/// working copies, unmerged ones and unreadable ones are left to [`recover_commits`].
pub(crate) fn finalize_pending_for_wc(store: &JournalStore, db: &WcDb) -> Result<()> {
    for path in store.list()? {
        let Ok(data) = JournalStore::read(&path) else {
            continue;
        };
        if data.merged.is_none() || data.wcroot != db.wcroot() {
            continue;
        }
        finalize(db, &data, &store.data_dir_of(&data.id))?;
        run_work_queue(db, &CancelToken::new())?;
        store.remove(&data.id)?;
    }
    Ok(())
}

async fn abort_journal(
    store: &JournalStore,
    data: &JournalData,
    ra: &dyn RaSession,
    cancel: &CancelToken,
    authenticated: &mut bool,
) -> Result<()> {
    if let Some(txn) = &data.txn {
        if !*authenticated {
            ra.ensure_authenticated(cancel).await?;
            *authenticated = true;
        }
        ra.abort_txn(txn).await?;
    }
    store.remove(&data.id)
}
