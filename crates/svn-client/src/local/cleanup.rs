//! `svn cleanup`: finish interrupted work and recover locks left by dead application processes.

use svn_types::{CancelToken, RelPath, Result};
use svn_wc::WcDb;
use svn_wc::db::{LockCleanup, ObtainMode};

use crate::wc::run_work_queue;

/// What [`cleanup`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Locks of dead application processes that were removed, and locks that need the user.
    pub locks: LockCleanup,
    /// Work items that were completed.
    pub work_items: usize,
    /// Unreferenced pristine texts that were deleted.
    pub pristines_removed: usize,
    /// Temp files of interrupted engine operations removed from `.svn/tmp`.
    pub tmp_files_removed: usize,
}

/// Cleans the working copy: recovers stale locks of dead application processes, takes the
/// write lock (failing with `WcBusy` when a foreign process holds it), completes the pending
/// work queue, deletes unreferenced pristine texts and removes the engine's own stale temp files
/// from `.svn/tmp` ([`WcDb::remove_stale_tmp_files`]).
pub fn cleanup(db: &WcDb, cancel: &CancelToken) -> Result<CleanupReport> {
    let locks = db.wc_cleanup_locks()?;
    let root = RelPath::root();
    db.wc_lock_obtain(&root, -1, ObtainMode::Cleanup)?;
    let result = (|| {
        let work_items = run_work_queue(db, cancel)?;
        let pristines_removed = db.pristine_store().remove_unreferenced()?;
        let tmp_files_removed = db.remove_stale_tmp_files()?;
        Ok(CleanupReport {
            locks,
            work_items,
            pristines_removed,
            tmp_files_removed,
        })
    })();
    let released = db.wc_lock_release(&root);
    match (result, released) {
        (Ok(r), Ok(())) => Ok(r),
        (Ok(_), Err(e)) | (Err(e), _) => Err(e),
    }
}
