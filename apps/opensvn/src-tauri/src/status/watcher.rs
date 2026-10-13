//! Recursive file system watcher feeding raw events into a channel.

use std::path::{Path, PathBuf};

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedSender;

/// One raw event: the path and whether it is (or was) a directory.
pub type RawEvent = (PathBuf, bool);

/// Keeps the OS watch alive; dropping it stops the watch.
pub struct FsWatcher {
    _inner: RecommendedWatcher,
}

/// Why starting a watch failed, with the one case users can fix called out.
#[derive(Debug)]
pub enum WatchError {
    /// Linux inotify watch limit reached (`fs.inotify.max_user_watches`).
    LimitReached,
    /// Any other failure.
    Other(notify::Error),
}

impl FsWatcher {
    /// Watches `root` recursively and sends events to `tx`. Access-only events are skipped.
    pub fn start(root: &Path, tx: UnboundedSender<RawEvent>) -> Result<Self, WatchError> {
        let mut inner = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                for path in event.paths {
                    let is_dir = path.is_dir();
                    let _ = tx.send((path, is_dir));
                }
            },
            Config::default(),
        )
        .map_err(classify)?;
        inner
            .watch(root, RecursiveMode::Recursive)
            .map_err(classify)?;
        Ok(Self { _inner: inner })
    }
}

fn classify(e: notify::Error) -> WatchError {
    if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
        WatchError::LimitReached
    } else {
        WatchError::Other(e)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn next_matching(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<RawEvent>,
        want: impl Fn(&Path) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some((p, _))) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if want(&p) {
                return true;
            }
        }
        false
    }

    #[tokio::test]
    async fn reports_changes_below_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _w = FsWatcher::start(&root, tx).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(root.join("src").join("main.rs"), b"fn main() {}").unwrap();
        assert!(next_matching(&mut rx, |p| p.ends_with("main.rs")).await);
    }

    #[test]
    fn missing_root_is_an_error_not_a_panic() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(FsWatcher::start(Path::new("/definitely/not/here"), tx).is_err());
    }
}
