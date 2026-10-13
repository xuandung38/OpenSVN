//! Crash recovery of the real engine when a working copy is opened after the app died in the
//! middle of a commit or an update. The crashes are injected with the `svn-client`/`svn-wc`
//! failpoints, so this runs only with `cargo test -p opensvn --features failpoints`; it is its
//! own test binary because failpoint configuration is process global.
#![cfg(feature = "failpoints")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use fail::FailScenario;
use opensvn_lib::engine::real::{
    ClientRecovery, ConnectOptions, RealEngine, RealEngineConfig, SessionConnector,
};
use opensvn_lib::engine::{EngineFacade, OpContext};
use svn_client::commit::journal::JournalStore;
use svn_client::commit::{CommitContext, CommitOptions, CommitResult};
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeMergeFault, FakeRaSession};
use svn_types::{
    CancelToken, Depth, ErrorCode, LockOwnershipStore, MemoryLockOwnership, RelPath, RepoUrl,
    SvnError,
};
use svn_wc::WcDb;
use svn_wc::db::{LockOwner, ObtainMode};

const URL: &str = "https://svn.test.local/repos/crash";
const THIS_BOOT: &str = "boot-now";
const OLD_BOOT: &str = "boot-before-crash";
const DEAD_PID: u32 = 4242;

/// Serves the in-memory repository (or nothing, when offline) and records the interactivity
/// every connection asked for.
struct Connector {
    repo: Option<FakeRaSession>,
    interactive: Mutex<Vec<bool>>,
}

#[async_trait]
impl SessionConnector for Connector {
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        self.connect_with(
            url,
            &ConnectOptions {
                anchor: Default::default(),
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
        self.interactive.lock().unwrap().push(options.interactive);
        match &self.repo {
            Some(r) => Ok(Arc::new(r.clone()) as Arc<dyn RaSession>),
            None => Err(SvnError::new(
                ErrorCode::Network,
                format!("cannot connect to {url}"),
            )),
        }
    }
}

struct Env {
    repo: FakeRaSession,
    connector: Arc<Connector>,
    engine: RealEngine,
    store: Arc<MemoryLockOwnership>,
    _state: tempfile::TempDir,
    _tmp: tempfile::TempDir,
    wc: PathBuf,
}

fn put(path: &str, text: &str) -> FakeChange {
    FakeChange::PutFile(RelPath::new(path).unwrap(), Bytes::from(text.to_owned()))
}

/// Repository with `trunk/a.txt` and `trunk/b.txt` at r1, `trunk` checked out; `online` says
/// whether the engine can reach the repository.
async fn env(online: bool) -> Env {
    let repo = FakeRaSession::new(URL, "crash-uuid").unwrap();
    repo.seed(
        "alice",
        "import",
        &[
            FakeChange::PutDir(RelPath::new("trunk").unwrap()),
            put("trunk/a.txt", "one\ntwo\nthree\n"),
            put("trunk/b.txt", "b\n"),
        ],
    )
    .unwrap();
    let connector = Arc::new(Connector {
        repo: online.then(|| repo.clone()),
        interactive: Mutex::new(Vec::new()),
    });
    let state = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryLockOwnership::for_current_process(THIS_BOOT));
    let engine = RealEngine::new(RealEngineConfig {
        connector: connector.clone(),
        lock_ownership: store.clone() as Arc<dyn LockOwnershipStore>,
        boot_id: THIS_BOOT.to_owned(),
        state_dir: state.path().to_path_buf(),
        recovery: Arc::new(ClientRecovery),
    });
    let tmp = tempfile::tempdir().unwrap();
    let wc = tmp.path().join("wc");
    svn_client::checkout(
        &engine.client_ctx(&OpContext::detached()),
        &trunk(&repo),
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    Env {
        repo,
        connector,
        engine,
        store,
        _state: state,
        _tmp: tmp,
        wc,
    }
}

fn trunk(repo: &FakeRaSession) -> FakeRaSession {
    repo.at(&RepoUrl::parse(&format!("{URL}/trunk")).unwrap())
        .unwrap()
}

impl Env {
    fn owner(&self) -> LockOwner {
        LockOwner {
            store: self.store.clone() as Arc<dyn LockOwnershipStore>,
            pid: std::process::id(),
            boot_id: THIS_BOOT.to_owned(),
        }
    }

    async fn commit(&self, message: &str) -> Result<Option<CommitResult>, SvnError> {
        let (db, _) = svn_client::wc::open_containing(&self.owner(), &self.wc)?;
        svn_client::commit::commit(
            &db,
            &CommitContext {
                ra: &self.repo,
                state_dir: &self.engine.commit_journal_dir(),
                cancel: &CancelToken::new(),
            },
            &[db.wcroot().to_path_buf()],
            &CommitOptions::new(message),
        )
        .await
    }

    async fn update(&self) -> Result<svn_client::UpdateSummary, SvnError> {
        svn_client::update(
            &self.engine.client_ctx(&OpContext::detached()),
            &trunk(&self.repo),
            &self.wc,
            None,
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
    }

    /// What a process killed while holding the working copy lock leaves: the lock row, its
    /// ownership record naming a process of an earlier boot, and the marker file.
    fn plant_dead_lock(&self) {
        let db = WcDb::open(&self.wc).unwrap();
        db.set_lock_owner(LockOwner {
            store: self.store.clone() as Arc<dyn LockOwnershipStore>,
            pid: DEAD_PID,
            boot_id: OLD_BOOT.to_owned(),
        });
        db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Cleanup)
            .unwrap();
    }

    fn journals(&self) -> Vec<PathBuf> {
        JournalStore::new(self.engine.commit_journal_dir())
            .list()
            .unwrap()
    }

    fn base_rev(&self, rel: &str) -> i64 {
        WcDb::open(&self.wc)
            .unwrap()
            .with_read(|c| {
                c.query_row(
                    "SELECT revision FROM nodes WHERE local_relpath = ?1 AND op_depth = 0",
                    [rel],
                    |r| r.get(0),
                )
                .map_err(svn_wc::db::sql_err)
            })
            .unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        WcDb::open(&self.wc)
            .unwrap()
            .with_read(|c| {
                c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .map_err(svn_wc::db::sql_err)
            })
            .unwrap()
    }

    async fn youngest(&self) -> u64 {
        self.repo.youngest_rev(&CancelToken::new()).await.unwrap().0
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Killed after the MERGE was sent and applied, before the revision reached the journal: the
/// next open releases the dead lock, finds the commit in the log over an unattended session and
/// finalizes the working copy. No manual step.
#[tokio::test]
async fn killed_after_the_merge_was_sent_is_finalized_on_the_next_open() {
    let _scenario = FailScenario::setup();
    let e = env(true).await;
    std::fs::write(e.wc.join("a.txt"), "changed\n").unwrap();
    fail::cfg("commit.journal_merged_write", "return").unwrap();
    fail::cfg("commit.after_merge_before_finalize", "return").unwrap();
    e.commit("edit a").await.expect_err("killed");
    fail::remove("commit.after_merge_before_finalize");
    fail::remove("commit.journal_merged_write");
    e.plant_dead_lock();
    assert_eq!(e.youngest().await, 2, "the revision exists");
    let journals = e.journals();
    assert_eq!(journals.len(), 1);
    let data = JournalStore::read(&journals[0]).unwrap();
    assert!(data.outcome_unknown(), "{data:?}");
    assert_eq!(e.base_rev("a.txt"), 1);

    let opened = e
        .engine
        .open_wc(&e.wc, &OpContext::detached())
        .await
        .unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert!(e.journals().is_empty());
    assert_eq!(e.base_rev("a.txt"), 2);
    assert_eq!(read(&e.wc.join("a.txt")), "changed\n");
    assert_eq!(e.count("wc_lock"), 0);
    assert_eq!(*e.connector.interactive.lock().unwrap(), vec![false]);

    // Recovery happens once, and the next commit builds on the finalized BASE.
    let again = e
        .engine
        .open_wc(&e.wc, &OpContext::detached())
        .await
        .unwrap();
    assert!(again.recovery.recovered.is_empty() && again.recovery.needs_user_action.is_empty());
    std::fs::write(e.wc.join("a.txt"), "next\n").unwrap();
    let res = e.commit("next").await.unwrap().unwrap();
    assert_eq!(res.revision.0, 3);
}

/// Killed after the revision reached the journal: finalizing needs no server, so it happens
/// even when the repository cannot be reached.
#[tokio::test]
async fn killed_after_the_merge_was_recorded_is_finalized_offline() {
    let _scenario = FailScenario::setup();
    let e = env(false).await;
    std::fs::write(e.wc.join("a.txt"), "changed\n").unwrap();
    fail::cfg("commit.after_merge_before_finalize", "return").unwrap();
    e.commit("edit a").await.expect_err("killed");
    fail::remove("commit.after_merge_before_finalize");
    e.plant_dead_lock();

    let opened = e
        .engine
        .open_wc(&e.wc, &OpContext::detached())
        .await
        .unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert!(e.journals().is_empty());
    assert_eq!(e.base_rev("a.txt"), 2);
    assert!(
        e.connector.interactive.lock().unwrap().is_empty(),
        "no connection needed"
    );
}

/// Killed after the MERGE was sent; the server never applied it, an administrator removed the
/// transaction and someone else committed since. Whether the commit happened cannot be told:
/// the journal stays, the open reports the working copy for the user and the next commit is
/// refused until it is resolved.
#[tokio::test]
async fn killed_after_the_merge_was_sent_with_an_unknown_outcome_needs_the_user() {
    let _scenario = FailScenario::setup();
    let e = env(true).await;
    std::fs::write(e.wc.join("a.txt"), "changed\n").unwrap();
    e.repo.fail_next_merge(FakeMergeFault::LostBeforeMerge);
    let err = e.commit("edit a").await.expect_err("lost");
    assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown, "{err}");
    let journal = e.journals().remove(0);
    let txn = JournalStore::read(&journal).unwrap().txn.unwrap();
    e.repo.abort_txn(&txn).await.unwrap();
    e.repo
        .seed("bob", "other", &[put("trunk/b.txt", "bob\n")])
        .unwrap();
    let before = std::fs::read(&journal).unwrap();

    for _ in 0..2 {
        let opened = e
            .engine
            .open_wc(&e.wc, &OpContext::detached())
            .await
            .unwrap();
        assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
        assert!(opened.recovery.recovered.is_empty());
    }
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        before,
        "journal untouched"
    );
    let refused = e.commit("again").await.expect_err("refused");
    assert_eq!(refused.code, ErrorCode::WcNeedsCleanup, "{refused}");
}

/// Killed in the middle of an update, after the new BASE was written and before its work queue
/// ran: the next open releases the dead lock and runs the queue, so the working file has the new
/// text without a manual cleanup; the next update completes the drive.
#[tokio::test(flavor = "multi_thread")]
async fn killed_mid_update_is_completed_on_the_next_open() {
    let _scenario = FailScenario::setup();
    let e = env(true).await;
    e.repo
        .seed("bob", "r2", &[put("trunk/b.txt", "b2\n")])
        .unwrap();
    // From the moment the transaction is written, every attempt to run the queue "dies".
    fail::cfg_callback("update.after_txn_before_queue", || {
        fail::cfg("wc.wq.after_delete", "return").unwrap();
    })
    .unwrap();
    let err = e.update().await.expect_err("killed");
    fail::remove("update.after_txn_before_queue");
    fail::remove("wc.wq.after_delete");
    assert!(err.message.contains("wc.wq.after_delete"), "{err}");
    e.plant_dead_lock();
    assert!(e.count("work_queue") > 0, "work left behind");
    assert_eq!(e.base_rev("b.txt"), 2, "the new BASE is in wc.db");
    assert_eq!(
        read(&e.wc.join("b.txt")),
        "b\n",
        "the file is not installed yet"
    );

    let opened = e
        .engine
        .open_wc(&e.wc, &OpContext::detached())
        .await
        .unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert_eq!(e.count("work_queue"), 0);
    assert_eq!(e.count("wc_lock"), 0);
    assert_eq!(read(&e.wc.join("b.txt")), "b2\n");

    let summary = e.update().await.unwrap();
    assert_eq!(summary.revision.map(|r| r.0), Some(2));
    assert_eq!(e.base_rev(""), 2);
}
