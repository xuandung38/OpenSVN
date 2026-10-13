//! Groups bursts of file system events into one list of dirty directories.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Quiet period after the last event before a batch is released.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// True when `path` is inside a working copy administrative area, which the app itself writes
/// during every operation and which must not trigger a rescan.
pub fn is_admin_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".svn")
}

/// Collects dirty directories and releases them once events stop arriving.
#[derive(Debug)]
pub struct Debouncer {
    quiet: Duration,
    dirty: BTreeSet<PathBuf>,
    last_event: Option<Instant>,
}

impl Debouncer {
    /// Debouncer with the given quiet period.
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            dirty: BTreeSet::new(),
            last_event: None,
        }
    }

    /// Records an event for `path` (the changed file or directory). Events inside `.svn` are
    /// dropped. The containing directory is what becomes dirty.
    pub fn push(&mut self, path: &Path, is_dir: bool, now: Instant) {
        if is_admin_path(path) {
            return;
        }
        let dir = if is_dir { Some(path) } else { path.parent() };
        if let Some(dir) = dir {
            self.dirty.insert(dir.to_path_buf());
            self.last_event = Some(now);
        }
    }

    /// When the current batch becomes ready, `None` when nothing is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.last_event
            .filter(|_| !self.dirty.is_empty())
            .map(|t| t + self.quiet)
    }

    /// Releases the batch if the quiet period has passed.
    pub fn take_ready(&mut self, now: Instant) -> Option<Vec<PathBuf>> {
        let deadline = self.deadline()?;
        if now < deadline {
            return None;
        }
        self.last_event = None;
        Some(std::mem::take(&mut self.dirty).into_iter().collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn burst_becomes_one_batch_after_the_quiet_period() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(DEBOUNCE);
        for i in 0..50u64 {
            d.push(
                Path::new("/wc/src/a.rs"),
                false,
                t0 + Duration::from_millis(i * 5),
            );
        }
        d.push(
            Path::new("/wc/docs/b.md"),
            false,
            t0 + Duration::from_millis(250),
        );
        // events keep postponing the batch
        assert!(d.take_ready(t0 + Duration::from_millis(400)).is_none());
        let batch = d.take_ready(t0 + Duration::from_millis(551)).unwrap();
        assert_eq!(
            batch,
            vec![PathBuf::from("/wc/docs"), PathBuf::from("/wc/src")]
        );
        assert!(d.take_ready(t0 + Duration::from_secs(5)).is_none());
        assert!(d.deadline().is_none());
    }

    #[test]
    fn admin_area_events_are_ignored() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(DEBOUNCE);
        d.push(Path::new("/wc/.svn/wc.db-journal"), false, t0);
        d.push(Path::new("/wc/.svn"), true, t0);
        assert!(d.deadline().is_none());
        assert!(is_admin_path(Path::new("/wc/a/.svn/pristine/x")));
        assert!(!is_admin_path(Path::new("/wc/a/.svnignore")));
    }

    #[test]
    fn directory_events_mark_the_directory_itself() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(Duration::from_millis(10));
        d.push(Path::new("/wc/new-dir"), true, t0);
        assert_eq!(
            d.take_ready(t0 + Duration::from_millis(10)).unwrap(),
            vec![PathBuf::from("/wc/new-dir")]
        );
    }
}
