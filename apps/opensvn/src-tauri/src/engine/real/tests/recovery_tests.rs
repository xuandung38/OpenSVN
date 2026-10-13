//! Opening a working copy: description, lock recovery, work queue and commit journals.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use opensvn_ipc::{Rev, WcFormat, WcId};
use svn_client::commit::journal::{JournalData, JournalStore};
use svn_ra_api::fake::FakeMergeFault;
use svn_ra_api::{CommitInfo, CommitObserver, RaSession};
use svn_types::{CancelToken, ErrorCode, LockOwnershipStore, RelPath, SvnError};
use svn_wc::WcDb;

use super::super::RecoveryHooks;
use super::support::{
    OLD_BOOT, ScriptedConnector, ScriptedSession, StaticConnector, URL, add_base_dir,
    add_base_file, checkout_trunk, client_commit, count_rows, ctx, fixture, fixture_on,
    fixture_with, make_wc, plant_dead_app_lock, plant_lock_row, plant_work_item, standard_repo,
};
use crate::engine::{EngineFacade, EngineResult, OpContext, WcRef};

/// Hooks that succeed and count how often they ran.
#[derive(Default)]
struct Finishing {
    queues: AtomicU32,
}

#[async_trait]
impl RecoveryHooks for Finishing {
    async fn run_work_queue(&self, db: &Arc<WcDb>, _ctx: &OpContext) -> EngineResult<()> {
        self.queues.fetch_add(1, Ordering::SeqCst);
        db.with_write(|tx| {
            tx.execute("DELETE FROM work_queue", [])
                .map(|_| ())
                .map_err(svn_wc::db::sql_err)
        })
    }
}

#[tokio::test]
async fn open_describes_the_working_copy() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    add_base_file(&root, "README.md", "trunk/README.md", 1);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    let info = opened.info;
    assert_eq!(info.repo_root_url, URL);
    assert_eq!(info.repo_uuid, "real-uuid");
    assert_eq!(info.url, format!("{URL}/trunk"));
    assert_eq!(info.path_in_repo, "/trunk");
    assert_eq!(info.base_rev, Rev(1));
    assert_eq!(info.rev_range, None);
    assert_eq!(info.format, WcFormat::V31);
    assert!(info.store_pristine);
    assert_eq!(opened.recovery.recovered, Vec::<String>::new());
    assert_eq!(opened.recovery.needs_user_action, Vec::<String>::new());
}

#[tokio::test]
async fn mixed_revisions_report_their_range() {
    let f = fixture();
    let (_tmp, root) = make_wc(3);
    add_base_file(&root, "README.md", "trunk/README.md", 1);
    let info = f.engine.wc_info(&root, &ctx()).await.unwrap();
    assert_eq!(info.rev_range, Some((Rev(1), Rev(3))));
}

#[tokio::test]
async fn directories_outside_a_working_copy_are_rejected() {
    let f = fixture();
    let tmp = tempfile::tempdir().unwrap();
    for result in [
        f.engine.wc_info(tmp.path(), &ctx()).await.map(|_| ()),
        f.engine.open_wc(tmp.path(), &ctx()).await.map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().code, ErrorCode::NotAWorkingCopy);
    }
}

/// Marker files of app locks below `.svn/tmp`.
fn markers(root: &Path) -> usize {
    std::fs::read_dir(root.join(".svn").join("tmp"))
        .map(|d| {
            d.filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("opensvn-lock-"))
            })
            .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn a_dead_app_lock_with_its_marker_is_released_once() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    add_base_dir(&root, "src", "trunk/src", 1);
    // A previous app run died while holding the lock on `src`: row, record and marker exist.
    plant_dead_app_lock(&root, &f.store, "src");
    assert_eq!((count_rows(&root, "wc_lock"), markers(&root)), (1, 1));

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec!["src".to_owned()]);
    assert!(opened.recovery.needs_user_action.is_empty());
    assert_eq!(count_rows(&root, "wc_lock"), 0);
    assert_eq!(markers(&root), 0, "the marker is swept with its row");
    assert!(f.store.owned_by_dead_process(&root).unwrap().is_empty());
    let again = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(again.recovery.recovered.is_empty(), "recovery happens once");
}

#[tokio::test]
async fn dead_owner_record_without_marker_keeps_the_row() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let src = RelPath::new("src").unwrap();
    // The registry names a dead app process, but no marker proves the row is the one it left:
    // svn or TortoiseSVN may have taken the directory since (svn cleanup empties .svn/tmp).
    plant_lock_row(&root, "src");
    f.store.record(&root, &src, 4242, OLD_BOOT).unwrap();

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.recovered.is_empty());
    assert_eq!(opened.recovery.needs_user_action, vec!["src".to_owned()]);
    assert_eq!(
        count_rows(&root, "wc_lock"),
        1,
        "never deleted without a marker"
    );
    let again = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(
        again.recovery.needs_user_action,
        vec!["src".to_owned()],
        "still waiting for the user"
    );
}

#[tokio::test]
async fn a_foreign_lock_is_reported_and_never_stolen() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    // Another program holds a lock the registry knows nothing about.
    plant_lock_row(&root, "src");
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.recovered.is_empty());
    assert_eq!(opened.recovery.needs_user_action, vec!["src".to_owned()]);
    assert_eq!(count_rows(&root, "wc_lock"), 1, "never stolen");
}

#[cfg(unix)]
#[tokio::test]
async fn locks_are_recovered_whichever_spelling_of_the_root_is_opened() {
    let f = fixture();
    let (tmp, root) = make_wc(1);
    add_base_dir(&root, "src", "trunk/src", 1);
    plant_dead_app_lock(&root, &f.store, "src");
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();

    let opened = f.engine.open_wc(&link, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec!["src".to_owned()]);
    assert_eq!(count_rows(&root, "wc_lock"), 0);
}

#[tokio::test]
async fn work_queue_runs_when_nobody_else_holds_a_lock() {
    let hooks = Arc::new(Finishing::default());
    let f = fixture_with(hooks.clone());
    let (_tmp, root) = make_wc(1);
    plant_work_item(&root);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert_eq!(count_rows(&root, "work_queue"), 0);
    let again = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(again.recovery.recovered.is_empty());
    assert_eq!(hooks.queues.load(Ordering::SeqCst), 1);

    // With a foreign lock in place the queue is not touched.
    plant_work_item(&root);
    plant_lock_row(&root, "");
    let blocked = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(blocked.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(hooks.queues.load(Ordering::SeqCst), 1);
    assert_eq!(count_rows(&root, "work_queue"), 1);
}

#[tokio::test]
async fn unfinished_work_that_cannot_run_is_reported_for_cleanup() {
    // The planted item is not a valid work item: running it fails and it stays.
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    plant_work_item(&root);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(count_rows(&root, "work_queue"), 1, "left for cleanup");
}

#[tokio::test]
async fn ensure_authenticated_signs_in_to_the_working_copy_repository_only_when_interactive() {
    let repo = standard_repo();
    let session = Arc::new(ScriptedSession::new(repo.clone(), None));
    let f = fixture_on(
        repo,
        Arc::new(ScriptedConnector(session.clone())),
        Arc::new(super::super::ClientRecovery),
    );
    let (_tmp, root) = make_wc(1);
    let wc = WcRef {
        id: WcId("auth".into()),
        root,
    };
    f.engine
        .ensure_authenticated(&wc, &OpContext::background())
        .await
        .unwrap();
    assert!(session.calls().is_empty(), "background work never prompts");
    f.engine.ensure_authenticated(&wc, &ctx()).await.unwrap();
    assert_eq!(session.calls(), vec!["auth"]);
}

// --- Commit journals (svn-client's, below `<state_dir>/commit-journals/`) ---------------------

/// Fixture whose sessions are the scripted session over the standard repository.
fn scripted(abort_error: Option<ErrorCode>) -> (super::support::Fixture, Arc<ScriptedSession>) {
    let repo = standard_repo();
    let session = Arc::new(ScriptedSession::new(repo.clone(), abort_error));
    let f = fixture_on(
        repo,
        Arc::new(ScriptedConnector(session.clone())),
        Arc::new(super::super::ClientRecovery),
    );
    (f, session)
}

/// Root of the working copy at `root` as the engine's handle names it (journals are keyed by it).
fn wcroot_of(root: &Path) -> PathBuf {
    WcDb::open(root).unwrap().wcroot().to_path_buf()
}

/// Writes a journal of the working copy `wcroot` that has reached `txn` (if any), with
/// `merge_sent` as given and nothing merged; returns its file.
fn plant_journal(
    f: &super::support::Fixture,
    wcroot: &Path,
    txn: Option<&str>,
    merge_sent: bool,
) -> PathBuf {
    let store = JournalStore::new(f.engine.commit_journal_dir());
    let id = store.new_id();
    let mut data: JournalData = store.begin(
        id.clone(),
        wcroot.to_path_buf(),
        false,
        false,
        Vec::new(),
        Vec::new(),
    );
    data.txn = txn.map(str::to_owned);
    data.merge_sent = merge_sent;
    data.start_youngest = Some(0);
    data.message = Some("lost commit".to_owned());
    store.write(&data).unwrap();
    store.file_of(&id)
}

/// Journal files present, sorted.
fn journals(f: &super::support::Fixture) -> Vec<PathBuf> {
    JournalStore::new(f.engine.commit_journal_dir())
        .list()
        .unwrap()
}

/// Names in the journal directory (journals, data directories, set-aside files), sorted.
fn journal_dir_listing(f: &super::support::Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(f.engine.commit_journal_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Observer that records nothing (a transaction left behind by a crashed commit).
struct Ignore;

impl CommitObserver for Ignore {
    fn on_txn_created(&self, _txn: &str) -> svn_types::Result<()> {
        Ok(())
    }
    fn on_merged(&self, _info: &CommitInfo) -> svn_types::Result<()> {
        Ok(())
    }
}

/// Opens a server transaction and abandons it, as a commit that died before MERGE does.
async fn leak_txn(repo: &svn_ra_api::fake::FakeRaSession) -> String {
    let editor = repo
        .commit_editor(
            "lost",
            &[],
            &[],
            false,
            Arc::new(Ignore),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    drop(editor);
    let txns = repo.open_txns();
    assert_eq!(txns.len(), 1);
    txns[0].clone()
}

#[tokio::test]
async fn opening_without_journals_never_connects() {
    let (f, session) = scripted(None);
    let (_tmp, root) = make_wc(1);
    let other = plant_journal(&f, Path::new("/elsewhere/wc"), Some("t"), true);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.recovered.is_empty() && opened.recovery.needs_user_action.is_empty());
    assert!(session.connects().is_empty(), "{:?}", session.connects());
    assert!(
        other.exists(),
        "another working copy's journal is not touched"
    );
}

#[tokio::test]
async fn a_leaked_server_transaction_is_deleted_over_an_unattended_session() {
    let (f, session) = scripted(None);
    let (_tmp, root) = make_wc(1);
    let txn = leak_txn(&f.repo).await;
    plant_journal(&f, &wcroot_of(&root), Some(&txn), false);

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.recovered.is_empty() && opened.recovery.needs_user_action.is_empty());
    assert!(f.repo.open_txns().is_empty(), "transaction deleted");
    assert!(journals(&f).is_empty(), "journal removed");
    assert_eq!(session.connects(), vec![false], "opening never prompts");
    assert_eq!(session.calls(), vec!["auth", "abort"], "signed in first");
}

#[tokio::test]
async fn an_unknown_merge_outcome_with_the_transaction_still_there_is_dropped() {
    let (f, _session) = scripted(None);
    let (_tmp, root) = make_wc(1);
    let txn = leak_txn(&f.repo).await;
    plant_journal(&f, &wcroot_of(&root), Some(&txn), true);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert!(f.repo.open_txns().is_empty());
    assert!(journals(&f).is_empty());
}

#[tokio::test]
async fn an_unknown_merge_outcome_the_log_does_not_settle_is_kept_for_the_user() {
    let (f, _session) = scripted(None);
    let (_tmp, root) = make_wc(1);
    // The transaction is gone and r1 is not this commit.
    let file = plant_journal(&f, &wcroot_of(&root), Some("gone"), true);
    let before = std::fs::read(&file).unwrap();
    for _ in 0..2 {
        let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
        assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
        assert!(opened.recovery.recovered.is_empty());
    }
    assert_eq!(std::fs::read(&file).unwrap(), before, "journal untouched");
}

#[tokio::test]
async fn a_temporary_abort_failure_keeps_the_journal_and_reports_it() {
    let (f, session) = scripted(Some(ErrorCode::Network));
    let (_tmp, root) = make_wc(1);
    plant_journal(&f, &wcroot_of(&root), Some("txn-1"), false);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journals(&f).len(), 1, "retried on the next open");
    assert_eq!(session.calls(), vec!["auth", "abort"]);
}

#[tokio::test]
async fn without_a_connection_the_journal_stays_and_is_reported() {
    let repo = standard_repo();
    let f = fixture_on(
        repo,
        Arc::new(StaticConnector(Vec::new())),
        Arc::new(super::super::ClientRecovery),
    );
    let (_tmp, root) = make_wc(1);
    plant_journal(&f, &wcroot_of(&root), Some("txn-1"), true);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journals(&f).len(), 1);
}

#[tokio::test]
async fn a_corrupt_journal_is_set_aside_and_reported() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let file = plant_journal(&f, &wcroot_of(&root), None, false);
    std::fs::write(&file, b"{ not json").unwrap();
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(!file.exists(), "no longer counts as a journal");
    let aside = file.with_extension("json.corrupt");
    assert_eq!(std::fs::read(aside).unwrap(), b"{ not json", "kept");
    let again = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(again.recovery.needs_user_action.is_empty(), "reported once");
}

#[tokio::test]
async fn a_journal_of_an_unknown_format_is_left_untouched() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let file = plant_journal(&f, &wcroot_of(&root), None, false);
    let contents = serde_json::json!({
        "version": 99,
        "wcroot": wcroot_of(&root),
        "something": "new",
    })
    .to_string();
    std::fs::write(&file, &contents).unwrap();
    let before = journal_dir_listing(&f);
    for _ in 0..2 {
        let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
        assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), contents);
    assert_eq!(journal_dir_listing(&f), before, "not moved aside");

    // Another working copy's journal of an unknown format is not this one's business.
    let (_tmp2, other) = make_wc(1);
    let opened = f.engine.open_wc(&other, &ctx()).await.unwrap();
    assert!(opened.recovery.needs_user_action.is_empty());
}

#[tokio::test]
async fn a_journal_that_cannot_be_read_is_kept_and_reported() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let file = plant_journal(&f, &wcroot_of(&root), None, false);
    // Something that lists as a journal but cannot be read as a file.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    std::fs::write(file.join("inside"), b"keep").unwrap();
    let before = journal_dir_listing(&f);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journal_dir_listing(&f), before, "nothing moved or removed");
    assert_eq!(std::fs::read(file.join("inside")).unwrap(), b"keep");
}

#[tokio::test]
async fn nothing_is_settled_while_a_foreign_lock_is_held() {
    let (f, session) = scripted(None);
    let (_tmp, root) = make_wc(1);
    let txn = leak_txn(&f.repo).await;
    plant_journal(&f, &wcroot_of(&root), Some(&txn), false);
    plant_lock_row(&root, "");
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journals(&f).len(), 1);
    assert_eq!(f.repo.open_txns(), vec![txn]);
    assert!(session.connects().is_empty());
}

/// Hooks whose work queue run always fails.
struct StuckQueue;

#[async_trait]
impl RecoveryHooks for StuckQueue {
    async fn run_work_queue(&self, _db: &Arc<WcDb>, _ctx: &OpContext) -> EngineResult<()> {
        Err(SvnError::new(ErrorCode::Internal, "stuck"))
    }
}

#[tokio::test]
async fn nothing_is_settled_over_unfinished_work() {
    let f = fixture_with(Arc::new(StuckQueue));
    let (_tmp, root) = make_wc(1);
    let txn = leak_txn(&f.repo).await;
    plant_journal(&f, &wcroot_of(&root), Some(&txn), false);
    plant_work_item(&root);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journals(&f).len(), 1);
    assert_eq!(f.repo.open_txns(), vec![txn]);
}

/// Path of the journal earlier app builds kept for the working copy at `root`.
fn legacy_file(f: &super::support::Fixture, root: &Path) -> PathBuf {
    use sha1::{Digest, Sha1};
    let canonical = std::fs::canonicalize(root).unwrap();
    let digest = Sha1::digest(canonical.to_string_lossy().as_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    f.state
        .path()
        .join("commit-journal")
        .join(format!("{name}.json"))
}

#[tokio::test]
async fn a_legacy_journal_is_set_aside_never_deleted_and_reported_once() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let file = legacy_file(&f, &root);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let contents = format!(
        r#"{{"version":1,"wc_root":"x","repo_root_url":"{URL}","phase":"merged","rev":2}}"#
    );
    std::fs::write(&file, &contents).unwrap();
    // An earlier set-aside copy is never replaced.
    let first = file.with_extension("json.legacy");
    std::fs::write(&first, b"older").unwrap();

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(opened.recovery.recovered.is_empty(), "nothing done from it");
    assert!(!file.exists());
    assert_eq!(std::fs::read(&first).unwrap(), b"older");
    assert_eq!(
        std::fs::read_to_string(file.with_extension("json.legacy.1")).unwrap(),
        contents
    );
    let again = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(again.recovery.needs_user_action.is_empty(), "reported once");
}

#[tokio::test]
async fn a_legacy_journal_that_cannot_be_set_aside_stays_and_keeps_being_reported() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let file = legacy_file(&f, &root);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"old").unwrap();
    std::fs::write(file.with_extension("json.legacy"), b"x").unwrap();
    for n in 1..8 {
        std::fs::write(file.with_extension(format!("json.legacy.{n}")), b"x").unwrap();
    }
    for _ in 0..2 {
        let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
        assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    }
    assert_eq!(std::fs::read(&file).unwrap(), b"old", "never deleted");
}

/// A commit whose MERGE was applied but whose answer was lost (the app was killed or the
/// connection dropped after the MERGE was sent): the next open finds it in the log and
/// finalizes the working copy without a manual step.
#[tokio::test]
async fn a_commit_applied_after_the_merge_was_sent_is_finalized_on_open() {
    let (f, session) = scripted(None);
    let (_tmp, root) = checkout_trunk(&f).await;
    std::fs::write(root.join("README.md"), "changed\n").unwrap();
    f.repo.fail_next_merge(FakeMergeFault::LostAfterMerge);
    let err = client_commit(&f, &root, &f.repo, "edit readme")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown, "{err}");
    assert_eq!(f.repo.youngest_rev(&CancelToken::new()).await.unwrap().0, 2);
    assert_eq!(journals(&f).len(), 1);

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert!(journals(&f).is_empty());
    assert_eq!(session.connects(), vec![false]);
    let base: i64 = WcDb::open(&root)
        .unwrap()
        .with_read(|c| {
            c.query_row(
                "SELECT revision FROM nodes WHERE local_relpath = 'README.md' AND op_depth = 0",
                [],
                |r| r.get(0),
            )
            .map_err(svn_wc::db::sql_err)
        })
        .unwrap();
    assert_eq!(base, 2, "the working copy has the committed revision");
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "changed\n"
    );

    // The next commit goes through.
    std::fs::write(root.join("README.md"), "again\n").unwrap();
    let res = client_commit(&f, &root, &f.repo, "again").await.unwrap();
    assert_eq!(res.unwrap().revision.0, 3);
}

/// The MERGE was sent but never applied: the open deletes the transaction and drops the
/// journal; the working copy keeps its local change for the next commit.
#[tokio::test]
async fn a_commit_lost_before_the_merge_is_dropped_on_open() {
    let (f, _session) = scripted(None);
    let (_tmp, root) = checkout_trunk(&f).await;
    std::fs::write(root.join("README.md"), "changed\n").unwrap();
    f.repo.fail_next_merge(FakeMergeFault::LostBeforeMerge);
    client_commit(&f, &root, &f.repo, "edit readme")
        .await
        .unwrap_err();
    assert_eq!(f.repo.open_txns().len(), 1);

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.needs_user_action.is_empty(), "{opened:?}");
    assert!(f.repo.open_txns().is_empty());
    assert!(journals(&f).is_empty());
    let res = client_commit(&f, &root, &f.repo, "retry").await.unwrap();
    assert_eq!(res.unwrap().revision.0, 2);
}

/// A merged commit is never finalized while another program holds a lock; once it is gone, the
/// next open finalizes.
#[tokio::test]
async fn a_merged_commit_waits_for_a_foreign_lock() {
    let (f, _session) = scripted(None);
    let (_tmp, root) = checkout_trunk(&f).await;
    std::fs::write(root.join("README.md"), "changed\n").unwrap();
    f.repo.fail_next_merge(FakeMergeFault::LostAfterMerge);
    client_commit(&f, &root, &f.repo, "edit readme")
        .await
        .unwrap_err();
    plant_lock_row(&root, "");
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journals(&f).len(), 1);

    WcDb::open(&root)
        .unwrap()
        .with_write(|tx| {
            tx.execute("DELETE FROM wc_lock", [])
                .map(|_| ())
                .map_err(svn_wc::db::sql_err)
        })
        .unwrap();
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert!(journals(&f).is_empty());
}
