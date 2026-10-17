//! Running the per-file disk work of a commit (hashing, copying, `fsync`) on a few threads.
//!
//! A commit of many small files spends most of its local time waiting for `fsync`. Syncs issued
//! from several threads at once overlap: the file system commits them together, so the batch
//! costs a few journal commits instead of one per file. Ordering is unchanged: every caller
//! waits for the whole batch before the step that depends on it.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use svn_types::{ErrorCode, Result, SvnError};

/// Upper bound of worker threads (the work is I/O bound).
const MAX_WORKERS: usize = 8;

/// Applies `f` to every item on up to [`MAX_WORKERS`] threads and returns the results in item
/// order. The first error is returned (the other items may or may not have run).
pub(crate) fn map<T, R, F>(items: Vec<T>, f: F) -> Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R> + Sync,
{
    let n = items.len();
    let workers = std::thread::available_parallelism()
        .map_or(2, |p| p.get() * 2)
        .clamp(1, MAX_WORKERS)
        .min(n);
    if workers <= 1 {
        return items.into_iter().map(f).collect();
    }
    let slots: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    let results: Vec<Mutex<Option<Result<R>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n || failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(item) = slots[i].lock().ok().and_then(|mut g| g.take()) else {
                        continue;
                    };
                    let r = f(item);
                    if r.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    if let Ok(mut g) = results[i].lock() {
                        *g = Some(r);
                    }
                }
            });
        }
    });
    let mut out = Vec::with_capacity(n);
    for r in results {
        match r.into_inner().ok().flatten() {
            Some(Ok(v)) => out.push(v),
            Some(Err(e)) => return Err(e),
            // Skipped after another item failed; that failure is reported below.
            None => {}
        }
    }
    if out.len() == n {
        Ok(out)
    } else {
        Err(SvnError::new(
            ErrorCode::Internal,
            "a parallel commit step stopped without reporting why",
        ))
    }
}

/// Flushes every file to disk (`fsync`), several at a time.
pub(crate) fn sync_files(paths: Vec<PathBuf>) -> Result<()> {
    map(paths, |p| {
        svn_wc::durable::sync_file(&p)
            .map_err(|e| SvnError::from_io(format!("sync '{}'", p.display()), &e))
    })
    .map(|_| ())
}

/// Flushes a directory entry (new files in it) to disk. Windows cannot open directories.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| SvnError::from_io(format!("sync directory '{}'", dir.display()), &e))
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_and_reports_the_first_error() {
        let out = map((0..100).collect(), |i: u32| Ok(i * 2)).unwrap();
        assert_eq!(out, (0..100).map(|i| i * 2).collect::<Vec<_>>());
        let err = map((0..100).collect(), |i: u32| {
            if i == 37 {
                Err(SvnError::new(ErrorCode::Corrupt, "bad 37"))
            } else {
                Ok(i)
            }
        })
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Corrupt);
        assert!(map(Vec::<u32>::new(), Ok).unwrap().is_empty());
    }
}
