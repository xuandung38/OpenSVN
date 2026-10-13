//! Write-lock scope shared by every mutating local operation.

use svn_types::{CancelToken, RelPath, Result};
use svn_wc::WcDb;
use svn_wc::db::{LockState, ObtainMode};

use super::wc_adapters::run_work_queue;

/// Runs `f` with the working copy write lock held on the whole tree.
///
/// A lock already held through this handle is reused (nested operations). Otherwise the lock is
/// taken in cleanup mode, leftover work items from an interrupted operation are finished first,
/// and the lock is released afterwards even when `f` fails. The caller must have authenticated
/// before calling this: no prompt may happen while the lock is held.
pub fn with_wc_lock<T>(
    db: &WcDb,
    cancel: &CancelToken,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let root = RelPath::root();
    if db.wc_lock_held(&root)? == LockState::HeldByUs {
        return f();
    }
    db.wc_lock_obtain(&root, -1, ObtainMode::Cleanup)?;
    let result = run_work_queue(db, cancel).and_then(|_| f());
    let released = db.wc_lock_release(&root);
    match (result, released) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(e)) | (Err(e), _) => Err(e),
    }
}

/// Runs `f` under the write lock, then finishes the work items it queued.
pub fn write_op<T>(db: &WcDb, cancel: &CancelToken, f: impl FnOnce() -> Result<T>) -> Result<T> {
    with_wc_lock(db, cancel, || {
        let value = f()?;
        run_work_queue(db, cancel)?;
        Ok(value)
    })
}
