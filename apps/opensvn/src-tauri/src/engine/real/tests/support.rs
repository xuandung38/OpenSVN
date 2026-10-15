//! Shared fixtures: an in-memory repository, a connector for it, and working copy builders.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{
    CommitEditor, CommitObserver, DirListing, FileMeta, FileRevHandler, LogRequest, RaSession,
    ReportFn, TxnAbort, UpdateRequest,
};
use svn_types::{
    CancelToken, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LockOwnershipStore, LogEntry,
    MemoryLockOwnership, NodeKind, PropChange, RelPath, RepoUrl, Revnum, SvnError,
};
use svn_wc::WcDb;
use svn_wc::db::{LockOwner, NewWc, ObtainMode};
use tokio::io::AsyncWrite;

use crate::engine::OpContext;
use crate::engine::real::{
    ClientRecovery, ConnectOptions, RealEngine, RealEngineConfig, RecoveryHooks, SessionConnector,
};

pub(super) const URL: &str = "https://svn.test.local/repos/real";
pub(super) const UUID: &str = "real-uuid";
/// Boot session id of the process that "crashed" in recovery tests.
pub(super) const OLD_BOOT: &str = "boot-before-crash";
/// Boot session id of the running process.
pub(super) const THIS_BOOT: &str = "boot-now";

/// Hands out sessions of in-memory repositories by URL prefix.
pub(super) struct StaticConnector(pub(super) Vec<FakeRaSession>);

#[async_trait]
impl SessionConnector for StaticConnector {
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        cancel.check()?;
        let wanted = url.to_string();
        self.0
            .iter()
            .find(|r| {
                let root = r.repos_root().to_string();
                wanted == root || wanted.starts_with(&format!("{root}/"))
            })
            .map(|r| Arc::new(r.clone()) as Arc<dyn RaSession>)
            .ok_or_else(|| {
                SvnError::new(
                    svn_types::ErrorCode::Network,
                    format!("cannot connect to {url}"),
                )
            })
    }
}

pub(super) fn put(path: &str, content: &str) -> FakeChange {
    FakeChange::PutFile(RelPath::new(path).unwrap(), Bytes::from(content.to_owned()))
}

pub(super) fn dir(path: &str) -> FakeChange {
    FakeChange::PutDir(RelPath::new(path).unwrap())
}

/// Repository in the state the shared engine suite expects (revision 1).
pub(super) fn standard_repo() -> FakeRaSession {
    let repo = FakeRaSession::new(URL, UUID).unwrap();
    repo.seed(
        "alice",
        "Initial import",
        &[
            dir("trunk"),
            dir("branches"),
            dir("tags"),
            put("trunk/README.md", "hello\n"),
            put("trunk/src/main.rs", "fn main() {}\n"),
        ],
    )
    .unwrap();
    repo
}

pub(super) struct Fixture {
    pub(super) repo: FakeRaSession,
    pub(super) engine: RealEngine,
    pub(super) state: tempfile::TempDir,
    pub(super) store: Arc<MemoryLockOwnership>,
}

pub(super) fn fixture() -> Fixture {
    fixture_with(Arc::new(ClientRecovery))
}

pub(super) fn fixture_with(recovery: Arc<dyn RecoveryHooks>) -> Fixture {
    let repo = standard_repo();
    let connector = Arc::new(StaticConnector(vec![repo.clone()]));
    fixture_on(repo, connector, recovery)
}

/// Fixture whose sessions come from `connector` (which must serve `repo`).
pub(super) fn fixture_on(
    repo: FakeRaSession,
    connector: Arc<dyn SessionConnector>,
    recovery: Arc<dyn RecoveryHooks>,
) -> Fixture {
    let state = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryLockOwnership::for_current_process(THIS_BOOT));
    let engine = RealEngine::new(RealEngineConfig {
        connector,
        lock_ownership: store.clone() as Arc<dyn LockOwnershipStore>,
        boot_id: THIS_BOOT.to_owned(),
        state_dir: state.path().to_path_buf(),
        recovery,
    });
    Fixture {
        repo,
        engine,
        state,
        store,
    }
}

pub(super) fn ctx() -> OpContext {
    OpContext::detached()
}

/// Creates a working copy of `trunk` at revision `rev` in a fresh temporary directory.
pub(super) fn make_wc(rev: u64) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wc");
    std::fs::create_dir(&root).unwrap();
    WcDb::create(
        &root,
        &NewWc {
            repos_root_url: URL.to_owned(),
            repos_uuid: UUID.to_owned(),
            repos_relpath: "trunk".to_owned(),
            revision: rev,
            depth: Depth::Infinity,
        },
    )
    .unwrap();
    (tmp, root)
}

/// Adds a BASE file node to the working copy at `root`.
pub(super) fn add_base_file(root: &Path, rel: &str, repos_path: &str, rev: i64) {
    add_base_node(root, rel, repos_path, rev, "file");
}

/// Adds a BASE directory node to the working copy at `root`.
pub(super) fn add_base_dir(root: &Path, rel: &str, repos_path: &str, rev: i64) {
    add_base_node(root, rel, repos_path, rev, "dir");
}

fn add_base_node(root: &Path, rel: &str, repos_path: &str, rev: i64, kind: &str) {
    let db = WcDb::open(root).unwrap();
    let wc_id = db.wc_id();
    let parent = rel.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
             repos_path, revision, presence, kind) \
             VALUES (?1, ?2, 0, ?3, 1, ?4, ?5, 'normal', ?6)",
            (wc_id, rel, &parent, repos_path, rev, kind),
        )
        .map(|_| ())
        .map_err(svn_wc::db::sql_err)
    })
    .unwrap();
}

/// Leaves a directory lock row in `wc.db`, as an interrupted writer would.
pub(super) fn plant_lock_row(root: &Path, dir: &str) {
    let db = WcDb::open(root).unwrap();
    let wc_id = db.wc_id();
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (?1, ?2, -1)",
            (wc_id, dir),
        )
        .map(|_| ())
        .map_err(svn_wc::db::sql_err)
    })
    .unwrap();
}

/// Leaves the complete lock of an app process that crashed while holding the lock on `dir`
/// (row, ownership record and marker file), exactly as `svn-wc` writes it.
pub(super) fn plant_dead_app_lock(root: &Path, store: &Arc<MemoryLockOwnership>, dir: &str) {
    let db = WcDb::open(root).unwrap();
    db.set_lock_owner(LockOwner {
        store: store.clone() as Arc<dyn LockOwnershipStore>,
        pid: DEAD_PID,
        boot_id: OLD_BOOT.to_owned(),
    });
    db.wc_lock_obtain(&RelPath::new(dir).unwrap(), -1, ObtainMode::Cleanup)
        .unwrap();
    // Dropped without release: the process "died" holding the lock.
}

/// Process id of the app run that "crashed" in recovery tests.
pub(super) const DEAD_PID: u32 = 4242;

/// Leaves an unfinished work queue item in `wc.db`.
pub(super) fn plant_work_item(root: &Path) {
    let db = WcDb::open(root).unwrap();
    db.with_write(|tx| {
        tx.execute("INSERT INTO work_queue (work) VALUES (X'28')", [])
            .map(|_| ())
            .map_err(svn_wc::db::sql_err)
    })
    .unwrap();
}

pub(super) fn count_rows(root: &Path, table: &str) -> i64 {
    let db = WcDb::open(root).unwrap();
    db.with_read(|c| {
        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(svn_wc::db::sql_err)
    })
    .unwrap()
}

/// Session that behaves like the in-memory repository except for `abort_txn` and
/// `abort_txn_checked`, which fail with `abort_error` when set; records the order of sign-in and
/// abort calls, and the interactivity every connection through [`ScriptedConnector`] asked for.
pub(super) struct ScriptedSession {
    pub(super) inner: FakeRaSession,
    pub(super) abort_error: Option<ErrorCode>,
    pub(super) calls: std::sync::Mutex<Vec<&'static str>>,
    /// `interactive` of every `connect_with` (`true` for a plain `connect`).
    pub(super) connects: std::sync::Mutex<Vec<bool>>,
    /// When set, `lock` and `unlock` never answer (a server that hangs).
    pub(super) hang_locks: std::sync::atomic::AtomicBool,
    /// Entries every listing reports as refused, like names the HTTP session will not accept.
    pub(super) refused_in_listings: std::sync::atomic::AtomicUsize,
}

impl ScriptedSession {
    pub(super) fn new(inner: FakeRaSession, abort_error: Option<ErrorCode>) -> Self {
        Self {
            inner,
            abort_error,
            calls: std::sync::Mutex::new(Vec::new()),
            connects: std::sync::Mutex::new(Vec::new()),
            hang_locks: std::sync::atomic::AtomicBool::new(false),
            refused_in_listings: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    pub(super) fn connects(&self) -> Vec<bool> {
        self.connects.lock().unwrap().clone()
    }
}

#[async_trait]
impl RaSession for ScriptedSession {
    fn repos_root(&self) -> &RepoUrl {
        self.inner.repos_root()
    }
    fn uuid(&self) -> &str {
        self.inner.uuid()
    }
    fn session_url(&self) -> &RepoUrl {
        self.inner.session_url()
    }
    fn authenticated_user(&self) -> Option<String> {
        self.inner.authenticated_user()
    }
    async fn ensure_authenticated(&self, cancel: &CancelToken) -> Result<(), SvnError> {
        self.calls.lock().unwrap().push("auth");
        self.inner.ensure_authenticated(cancel).await
    }
    async fn youngest_rev(&self, cancel: &CancelToken) -> Result<Revnum, SvnError> {
        self.inner.youngest_rev(cancel).await
    }
    async fn check_path(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<NodeKind, SvnError> {
        self.inner.check_path(path, rev, cancel).await
    }
    async fn stat(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Option<DirEntry>, SvnError> {
        self.inner.stat(path, rev, cancel).await
    }
    async fn list(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<DirEntry>, SvnError> {
        self.inner.list(path, rev, depth, cancel).await
    }
    async fn list_counted(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<DirListing, SvnError> {
        Ok(DirListing {
            entries: self.inner.list(path, rev, depth, cancel).await?,
            rejected: self
                .refused_in_listings
                .load(std::sync::atomic::Ordering::SeqCst),
        })
    }
    async fn get_file(
        &self,
        path: &RelPath,
        rev: Option<Revnum>,
        sink: &mut (dyn AsyncWrite + Send + Unpin),
        cancel: &CancelToken,
    ) -> Result<FileMeta, SvnError> {
        self.inner.get_file(path, rev, sink, cancel).await
    }
    async fn log(
        &self,
        req: &LogRequest,
        on_entry: &mut (dyn FnMut(LogEntry) -> Result<(), SvnError> + Send),
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        self.inner.log(req, on_entry, cancel).await
    }
    async fn get_locks(
        &self,
        path: &RelPath,
        depth: Depth,
        cancel: &CancelToken,
    ) -> Result<Vec<LockInfo>, SvnError> {
        self.inner.get_locks(path, depth, cancel).await
    }
    async fn lock(
        &self,
        path: &RelPath,
        comment: Option<&str>,
        steal: bool,
        base_rev: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<LockInfo, SvnError> {
        if self.hang_locks.load(std::sync::atomic::Ordering::SeqCst) {
            return std::future::pending().await;
        }
        self.inner
            .lock(path, comment, steal, base_rev, cancel)
            .await
    }
    async fn unlock(
        &self,
        path: &RelPath,
        token: Option<&str>,
        break_lock: bool,
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        if self.hang_locks.load(std::sync::atomic::Ordering::SeqCst) {
            return std::future::pending().await;
        }
        self.inner.unlock(path, token, break_lock, cancel).await
    }
    async fn file_revs(
        &self,
        path: &RelPath,
        start: Option<Revnum>,
        end: Option<Revnum>,
        handler: &mut dyn FileRevHandler,
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        self.inner
            .file_revs(path, start, end, handler, cancel)
            .await
    }
    async fn get_location_segments(
        &self,
        path: &RelPath,
        peg: Option<Revnum>,
        start: Option<Revnum>,
        end: Option<Revnum>,
        cancel: &CancelToken,
    ) -> Result<Vec<svn_ra_api::LocationSegment>, SvnError> {
        self.inner
            .get_location_segments(path, peg, start, end, cancel)
            .await
    }
    async fn do_update(
        &self,
        req: UpdateRequest,
        report: ReportFn,
        editor: Box<dyn DeltaEditor>,
        cancel: &CancelToken,
    ) -> Result<(), SvnError> {
        self.inner.do_update(req, report, editor, cancel).await
    }
    async fn commit_editor(
        &self,
        message: &str,
        rev_props: &[PropChange],
        lock_tokens: &[(String, String)],
        keep_locks: bool,
        observer: Arc<dyn CommitObserver>,
        cancel: &CancelToken,
    ) -> Result<Box<dyn CommitEditor>, SvnError> {
        self.inner
            .commit_editor(
                message,
                rev_props,
                lock_tokens,
                keep_locks,
                observer,
                cancel,
            )
            .await
    }
    async fn abort_txn(&self, txn_name: &str) -> Result<(), SvnError> {
        self.calls.lock().unwrap().push("abort");
        match self.abort_error {
            Some(code) => Err(SvnError::new(code, format!("abort {txn_name} failed"))),
            None => self.inner.abort_txn(txn_name).await,
        }
    }
    async fn abort_txn_checked(
        &self,
        txn_name: &str,
        cancel: &CancelToken,
    ) -> Result<TxnAbort, SvnError> {
        self.calls.lock().unwrap().push("abort");
        match self.abort_error {
            Some(code) => Err(SvnError::new(code, format!("abort {txn_name} failed"))),
            None => self.inner.abort_txn_checked(txn_name, cancel).await,
        }
    }
}

/// Connector that always hands out the same scripted session.
pub(super) struct ScriptedConnector(pub(super) Arc<ScriptedSession>);

#[async_trait]
impl SessionConnector for ScriptedConnector {
    async fn connect(
        &self,
        _url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        cancel.check()?;
        self.0.connects.lock().unwrap().push(true);
        Ok(self.0.clone() as Arc<dyn RaSession>)
    }

    async fn connect_with(
        &self,
        _url: &RepoUrl,
        options: &ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        cancel.check()?;
        self.0.connects.lock().unwrap().push(options.interactive);
        Ok(self.0.clone() as Arc<dyn RaSession>)
    }
}

/// Checks out `trunk` of the fixture's repository with `svn-client` into a fresh temporary
/// directory (a working copy with pristines, as the app makes them).
pub(super) async fn checkout_trunk(f: &Fixture) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("wc");
    let trunk = f
        .repo
        .at(&RepoUrl::parse(&format!("{URL}/trunk")).unwrap())
        .unwrap();
    svn_client::checkout(
        &f.engine.client_ctx(&ctx()),
        &trunk,
        &root,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    (tmp, root)
}

/// Commits everything below `root` with `svn-client` over `ra` (rooted at the repository root),
/// journaled where the engine's recovery looks.
pub(super) async fn client_commit(
    f: &Fixture,
    root: &Path,
    ra: &dyn RaSession,
    message: &str,
) -> Result<Option<svn_client::commit::CommitResult>, SvnError> {
    let (db, _) = svn_client::wc::open_containing(&f.engine.lock_owner(), root)?;
    let state_dir = f.engine.commit_journal_dir();
    svn_client::commit::commit(
        &db,
        &svn_client::commit::CommitContext {
            ra,
            state_dir: &state_dir,
            cancel: &CancelToken::new(),
        },
        &[db.wcroot().to_path_buf()],
        &svn_client::commit::CommitOptions::new(message),
    )
    .await
}

/// Serves the in-memory repository like the HTTP connector: anchored at the requested URL for
/// [`Anchor::Url`](crate::engine::real::Anchor::Url) (`FakeRaSession::at`), at the root
/// otherwise. Records the interactivity of every connection; unattended connections fail while
/// `offline_unattended` is set (no session for background recovery).
pub(super) struct AnchoredConnector {
    pub(super) repo: FakeRaSession,
    pub(super) offline_unattended: std::sync::atomic::AtomicBool,
    pub(super) connects: std::sync::Mutex<Vec<bool>>,
}

impl AnchoredConnector {
    pub(super) fn new(repo: FakeRaSession) -> Self {
        Self {
            repo,
            offline_unattended: std::sync::atomic::AtomicBool::new(false),
            connects: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(super) fn connects(&self) -> Vec<bool> {
        self.connects.lock().unwrap().clone()
    }
}

#[async_trait]
impl SessionConnector for AnchoredConnector {
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        self.connect_with(
            url,
            &ConnectOptions {
                anchor: crate::engine::real::Anchor::Root,
                interactive: true,
            },
            cancel,
        )
        .await
    }

    async fn connect_with(
        &self,
        url: &RepoUrl,
        options: &ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        cancel.check()?;
        self.connects.lock().unwrap().push(options.interactive);
        if !options.interactive
            && self
                .offline_unattended
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(SvnError::new(
                ErrorCode::Network,
                format!("cannot connect to {url}"),
            ));
        }
        Ok(match options.anchor {
            crate::engine::real::Anchor::Root => Arc::new(self.repo.clone()),
            crate::engine::real::Anchor::Url => Arc::new(self.repo.at(url)?),
        })
    }
}
