//! Shared reads of repository logs: concurrent identical requests run once (single flight), and
//! finished pages are kept until the working copy's revision moves (update, commit, switch) or a
//! remote check sees new revisions.
//!
//! After an update the History bar, the History screen and the Incoming list each ask for a log at
//! the same moment; behind authz a page that reaches a revision with many changed paths can take
//! tens of seconds on the server, so asking once matters more than anything done locally.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use opensvn_ipc::{AppError, LogGetRequest, LogPage};
use tokio::sync::OnceCell;

use crate::engine::LogScope;

/// Pages that start at HEAD go stale when someone else commits; they are reused this long.
pub const HEAD_TTL: Duration = Duration::from_secs(120);

/// Finished pages kept (oldest dropped first).
const MAX_PAGES: usize = 64;

/// Everything that decides the answer of a log request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LogKey {
    /// Repository URL of the logged node.
    url: String,
    from: Option<u64>,
    to: Option<u64>,
    limit: u32,
    stop_on_copy: bool,
    with_paths: bool,
    filter: Option<String>,
    include_merged: bool,
    revprops: Option<Vec<String>>,
}

impl LogKey {
    /// Key of `req` read from `scope`: a working copy node and a URL naming the same location
    /// share it, and absent options read as their defaults.
    pub fn new(scope: &LogScope, req: &LogGetRequest) -> Self {
        let url = match scope {
            LogScope::Url(u) => u.trim_end_matches('/').to_owned(),
            LogScope::Repo(loc) if loc.repos_path.is_empty() => {
                loc.repo_root.trim_end_matches('/').to_owned()
            }
            LogScope::Repo(loc) => format!(
                "{}/{}",
                loc.repo_root.trim_end_matches('/'),
                loc.repos_path.trim_matches('/')
            ),
        };
        Self {
            url,
            from: req.from_rev.map(|r| r.0),
            to: req.to_rev.map(|r| r.0),
            limit: req.limit,
            stop_on_copy: req.stop_on_copy.unwrap_or(false),
            with_paths: req.with_paths,
            filter: req
                .filter
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_lowercase),
            include_merged: req.include_merged.unwrap_or(false),
            revprops: req.revprops.clone(),
        }
    }

    /// The same request with changed paths: its answer also answers this one.
    fn with_paths(&self) -> Option<Self> {
        (!self.with_paths).then(|| Self {
            with_paths: true,
            ..self.clone()
        })
    }
}

type Shared = Arc<OnceCell<Result<Arc<LogPage>, AppError>>>;

struct Done {
    page: Arc<LogPage>,
    at: Instant,
}

#[derive(Default)]
struct Inner {
    /// Bumped by [`LogCache::invalidate`]: flights started before do not store their pages.
    generation: u64,
    flights: HashMap<LogKey, (u64, Shared)>,
    done: HashMap<LogKey, Done>,
    order: VecDeque<LogKey>,
}

/// See the module documentation.
#[derive(Default)]
pub struct LogCache {
    inner: Mutex<Inner>,
    /// Requests that reached the server (tests and diagnostics).
    fetches: std::sync::atomic::AtomicU64,
}

/// Drops the changed paths of a page read with them, for a request without.
fn without_paths(page: &LogPage) -> LogPage {
    let mut p = page.clone();
    for e in &mut p.entries {
        e.changed_paths = None;
    }
    p
}

impl LogCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets every finished page; requests already running are not joined any more.
    pub fn invalidate(&self) {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.flights.clear();
        inner.done.clear();
        inner.order.clear();
    }

    /// Number of requests that ran `fetch` so far.
    #[cfg(test)]
    pub fn fetches(&self) -> u64 {
        self.fetches.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn cached(inner: &Inner, key: &LogKey) -> Option<Arc<LogPage>> {
        let d = inner.done.get(key)?;
        (key.from.is_some() || d.at.elapsed() < HEAD_TTL).then(|| Arc::clone(&d.page))
    }

    /// The page for `key`: from the cache, from an identical request already running, or from
    /// `fetch`.
    pub async fn get<F, Fut>(&self, key: LogKey, fetch: F) -> Result<LogPage, AppError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<LogPage, AppError>>,
    {
        let (generation, shared, strip) = {
            let mut inner = self.lock();
            if let Some(p) = Self::cached(&inner, &key) {
                return Ok((*p).clone());
            }
            let wider = key.with_paths();
            if let Some(p) = wider.as_ref().and_then(|w| Self::cached(&inner, w)) {
                return Ok(without_paths(&p));
            }
            let joined = inner
                .flights
                .get(&key)
                .map(|f| (f.1.clone(), false))
                .or_else(|| {
                    wider
                        .as_ref()
                        .and_then(|w| inner.flights.get(w))
                        .map(|f| (f.1.clone(), true))
                });
            match joined {
                Some((shared, strip)) => (inner.generation, shared, strip),
                None => {
                    let shared: Shared = Arc::new(OnceCell::new());
                    let generation = inner.generation;
                    inner
                        .flights
                        .insert(key.clone(), (generation, Arc::clone(&shared)));
                    (generation, shared, false)
                }
            }
        };
        let result = shared
            .get_or_init(|| async {
                self.fetches
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                fetch().await.map(Arc::new)
            })
            .await
            .clone();
        if !strip {
            self.finish(&key, generation, &shared, &result);
        }
        result.map(|p| {
            if strip {
                without_paths(&p)
            } else {
                (*p).clone()
            }
        })
    }

    /// Stores the page of a finished flight (once, by whichever waiter gets here first) unless
    /// the cache was invalidated since it started.
    fn finish(
        &self,
        key: &LogKey,
        generation: u64,
        shared: &Shared,
        result: &Result<Arc<LogPage>, AppError>,
    ) {
        let mut inner = self.lock();
        let ours = inner
            .flights
            .get(key)
            .is_some_and(|(g, s)| *g == generation && Arc::ptr_eq(s, shared));
        if !ours {
            return;
        }
        inner.flights.remove(key);
        if let Ok(page) = result {
            inner.done.insert(
                key.clone(),
                Done {
                    page: Arc::clone(page),
                    at: Instant::now(),
                },
            );
            inner.order.retain(|k| k != key);
            inner.order.push_back(key.clone());
            while inner.order.len() > MAX_PAGES {
                if let Some(old) = inner.order.pop_front() {
                    inner.done.remove(&old);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::engine::RepoLocation;
    use opensvn_ipc::{ChangedPath, LogEntry, LogTarget, Rev};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn req(with_paths: bool) -> LogGetRequest {
        LogGetRequest {
            target: LogTarget::Url {
                url: "https://svn.example.com/r".into(),
            },
            from_rev: None,
            to_rev: None,
            limit: 100,
            stop_on_copy: None,
            with_paths,
            filter: None,
            include_merged: None,
            revprops: None,
        }
    }

    fn page(with_paths: bool) -> LogPage {
        LogPage {
            entries: vec![LogEntry {
                rev: Rev(3),
                author: None,
                date: String::new(),
                message: "m".into(),
                changed_paths: with_paths.then(|| {
                    vec![ChangedPath {
                        action: opensvn_ipc::ChangeAction::M,
                        path: "/a".into(),
                        kind: opensvn_ipc::PathKind::File,
                        copy_from: None,
                        text_mods: None,
                        prop_mods: None,
                    }]
                }),
                has_children: None,
                merged: None,
                subtractive_merge: None,
                revprops: None,
            }],
            next_rev: None,
        }
    }

    #[test]
    fn a_url_and_a_working_copy_node_at_the_same_location_share_a_key() {
        let a = LogKey::new(&LogScope::Url("https://h/r/trunk/".into()), &req(false));
        let b = LogKey::new(
            &LogScope::Repo(RepoLocation {
                repo_root: "https://h/r".into(),
                repos_path: "trunk".into(),
            }),
            &LogGetRequest {
                stop_on_copy: Some(false),
                filter: Some("  ".into()),
                ..req(false)
            },
        );
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn concurrent_identical_requests_reach_the_server_once() {
        let cache = Arc::new(LogCache::default());
        let calls = Arc::new(AtomicU32::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let key = LogKey::new(&LogScope::Url("https://h/r".into()), &req(true));
        let tasks: Vec<_> = (0..3)
            .map(|i| {
                let (cache, calls, gate) = (cache.clone(), calls.clone(), gate.clone());
                // One of them asks without paths: the request with paths answers it too.
                let key = if i == 2 {
                    LogKey::new(&LogScope::Url("https://h/r".into()), &req(false))
                } else {
                    key.clone()
                };
                tokio::spawn(async move {
                    cache
                        .get(key, || async {
                            calls.fetch_add(1, Ordering::SeqCst);
                            gate.notified().await;
                            Ok(page(true))
                        })
                        .await
                })
            })
            .collect();
        // Let the first request start, then the others join it.
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        gate.notify_waiters();
        let mut got = Vec::new();
        for t in tasks {
            got.push(t.await.unwrap().unwrap());
        }
        assert!(calls.load(Ordering::SeqCst) <= 2);
        // Later requests come from the cache.
        let again = cache
            .get(key.clone(), || async { panic!("served from the cache") })
            .await
            .unwrap();
        assert_eq!(again, page(true));
        let narrow = LogKey::new(&LogScope::Url("https://h/r".into()), &req(false));
        let stripped = cache
            .get(narrow, || async { panic!("served from the cache") })
            .await
            .unwrap();
        assert_eq!(stripped, page(false));
    }

    #[tokio::test]
    async fn invalidation_drops_pages_and_running_flights_do_not_store() {
        let cache = Arc::new(LogCache::default());
        let key = LogKey::new(&LogScope::Url("https://h/r".into()), &req(false));
        cache
            .get(key.clone(), || async { Ok(page(false)) })
            .await
            .unwrap();
        cache.invalidate();
        // A request running while the cache is invalidated (an update finishing meanwhile).
        let gate = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let running = {
            let (cache, key, gate, started) =
                (cache.clone(), key.clone(), gate.clone(), started.clone());
            tokio::spawn(async move {
                cache
                    .get(key, || async move {
                        started.notify_one();
                        gate.notified().await;
                        Ok(page(false))
                    })
                    .await
            })
        };
        started.notified().await;
        cache.invalidate();
        gate.notify_one();
        running.await.unwrap().unwrap();
        assert_eq!(cache.fetches(), 2);
        // Its page was not kept: the next request reads again.
        cache.get(key, || async { Ok(page(false)) }).await.unwrap();
        assert_eq!(cache.fetches(), 3);
    }

    #[tokio::test]
    async fn errors_are_shared_but_not_cached() {
        let cache = LogCache::default();
        let key = LogKey::new(&LogScope::Url("https://h/r".into()), &req(false));
        let err = cache
            .get(key.clone(), || async {
                Err(AppError::from_code(svn_types::ErrorCode::Network, "down"))
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, opensvn_ipc::ErrorCode::Network);
        let ok = cache.get(key, || async { Ok(page(false)) }).await.unwrap();
        assert_eq!(ok, page(false));
    }
}
