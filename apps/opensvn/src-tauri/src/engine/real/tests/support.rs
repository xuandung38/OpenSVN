//! Shared fixtures: an in-memory repository, a connector for it, and working copy builders.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{
    CommitEditor, CommitObserver, FileMeta, FileRevHandler, LogRequest, RaSession, ReportFn,
    UpdateRequest,
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
    RealEngine, RealEngineConfig, RecoveryHooks, SessionConnector, UnwiredRecovery,
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
    fixture_with(Arc::new(UnwiredRecovery))
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

/// Session that behaves like the in-memory repository except for `abort_txn`, which fails with
/// `abort_error` when set; records the order of sign-in and abort calls.
pub(super) struct ScriptedSession {
    pub(super) inner: FakeRaSession,
    pub(super) abort_error: Option<ErrorCode>,
    pub(super) calls: std::sync::Mutex<Vec<&'static str>>,
}

impl ScriptedSession {
    pub(super) fn new(inner: FakeRaSession, abort_error: Option<ErrorCode>) -> Self {
        Self {
            inner,
            abort_error,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(super) fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
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
        Ok(self.0.clone() as Arc<dyn RaSession>)
    }
}
