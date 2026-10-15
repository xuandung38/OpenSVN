//! End-to-end latency of the status watcher (#11) on a 10k-file working copy: a file edit must
//! reach the app as a `wc://status-changed` delta in under a second. The path is the real one:
//! the OS watch (`status::watcher`), the 300 ms debounce and a full rescan through the real engine
//! (`status::service` + `status::publisher`), on a working copy checked out by `svn-client`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth};

use super::support::{StaticConnector, ctx, fixture_on, put};
use crate::db::{AppDb, CacheDb};
use crate::engine::EngineFacade;
use crate::engine::real::ClientRecovery;
use crate::ops::events::{Recorded, RecordingEvents};
use crate::status::{StatusPublisher, StatusService, StatusTiming};
use crate::wc::WcRegistry;

const URL: &str = "https://svn.test.local/repos/big";
const DIRS: usize = 100;
const FILES_PER_DIR: usize = 100;
/// The latency budget from the issue.
const BUDGET: Duration = Duration::from_secs(1);

fn big_repo() -> FakeRaSession {
    let repo = FakeRaSession::new(URL, "big-uuid").unwrap();
    let changes: Vec<_> = (0..DIRS)
        .flat_map(|d| {
            (0..FILES_PER_DIR).map(move |f| put(&format!("d{d:03}/f{f:03}.txt"), "line\n"))
        })
        .collect();
    repo.seed("alice", "10k files", &changes).unwrap();
    repo
}

/// Waits until a status delta lists `path`; the time it was seen, `None` after `limit`.
async fn delta_for(events: &RecordingEvents, path: &str, limit: Duration) -> Option<Instant> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        let seen = events.all().iter().any(|r| match r {
            Recorded::Status(s) => s.changed.iter().any(|c| c.path == path),
            _ => false,
        });
        if seen {
            return Some(Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_edit_in_a_10k_file_working_copy_is_announced_within_a_second() {
    let repo = big_repo();
    let connector = Arc::new(StaticConnector(vec![repo.clone()]));
    let f = fixture_on(repo, connector, Arc::new(ClientRecovery));
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wc");
    svn_client::checkout(
        &f.engine.client_ctx(&ctx()),
        &f.repo,
        &root,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    let root = root.canonicalize().unwrap();
    let files = walkdir_count(&root);
    assert!(files >= DIRS * FILES_PER_DIR, "{files} files on disk");

    let engine: Arc<dyn EngineFacade> = Arc::new(f.engine);
    let registry = WcRegistry::new(AppDb::open_in_memory().unwrap(), Arc::clone(&engine));
    let wc = registry
        .add_existing(root.to_str().unwrap())
        .await
        .expect("register");
    let resolved = registry.resolve(&wc.id).unwrap();
    let events = Arc::new(RecordingEvents::default());
    let publisher = Arc::new(StatusPublisher::new(
        Arc::clone(&engine),
        CacheDb::open_in_memory().unwrap(),
        events.clone(),
    ));
    // Opening a working copy publishes the first snapshot before the watch starts.
    let scan = Instant::now();
    publisher.publish(&resolved).await.unwrap();
    let full_scan = scan.elapsed();
    let service = StatusService::new(Arc::clone(&publisher), StatusTiming::default());
    service.start(resolved);

    // Warm-up edits until one is reported: the watch is set up by the spawned task, so an edit
    // made before it exists is never seen. After this the measured edits hit a live watch.
    let mut warmed = false;
    for i in 0..30 {
        std::fs::write(root.join("d000/f000.txt"), format!("line\nwarm-up {i}\n")).unwrap();
        if delta_for(&events, "d000/f000.txt", Duration::from_secs(1))
            .await
            .is_some()
        {
            warmed = true;
            break;
        }
    }
    assert!(warmed, "the watch never reported the warm-up edit");

    let mut worst = Duration::ZERO;
    for (i, path) in ["d050/f050.txt", "d099/f099.txt", "d007/f042.txt"]
        .iter()
        .enumerate()
    {
        let edited = Instant::now();
        std::fs::write(root.join(path), format!("line\nedit {i}\n")).unwrap();
        let seen = delta_for(&events, path, Duration::from_secs(10))
            .await
            .unwrap_or_else(|| panic!("no status delta for {path} within 10 s"));
        let latency = seen - edited;
        eprintln!("{path}: status delta after {latency:?} (full scan {full_scan:?})");
        worst = worst.max(latency);
    }
    service.stop(&wc.id.0);
    assert!(
        worst < BUDGET,
        "slowest status delta took {worst:?} (budget {BUDGET:?}, full scan {full_scan:?})"
    );
}

fn walkdir_count(root: &std::path::Path) -> usize {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            if e.file_name() == ".svn" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}
