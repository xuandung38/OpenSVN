//! Shared fixtures: an in-memory repository, a connector for it, and working copy builders.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{
    CancelToken, Depth, LockOwnershipStore, MemoryLockOwnership, RelPath, RepoUrl, SvnError,
};
use svn_wc::WcDb;
use svn_wc::db::NewWc;

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
    let state = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryLockOwnership::for_current_process(THIS_BOOT));
    let engine = RealEngine::new(RealEngineConfig {
        connector: Arc::new(StaticConnector(vec![repo.clone()])),
        lock_ownership: store.clone() as Arc<dyn LockOwnershipStore>,
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
    let db = WcDb::open(root).unwrap();
    let wc_id = db.wc_id();
    let parent = rel.rsplit_once('/').map_or("", |(p, _)| p).to_owned();
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO nodes (wc_id, local_relpath, op_depth, parent_relpath, repos_id, \
             repos_path, revision, presence, kind) \
             VALUES (?1, ?2, 0, ?3, 1, ?4, ?5, 'normal', 'file')",
            (wc_id, rel, &parent, repos_path, rev),
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
