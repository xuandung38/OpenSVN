//! Opening a working copy: description, lock recovery, work queue and commit journal.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use opensvn_ipc::{Rev, WcFormat};
use svn_ra_api::{CommitObserver, RaSession};
use svn_types::{CancelToken, ErrorCode, LockOwnershipStore, RelPath};
use svn_wc::WcDb;

use super::super::RecoveryHooks;
use super::super::UnwiredRecovery;
use super::super::journal::{JournalLoadError, JournalPhase, JournalRecord, JournalStore};
use super::support::{
    OLD_BOOT, ScriptedConnector, ScriptedSession, URL, add_base_dir, add_base_file, count_rows,
    ctx, fixture, fixture_on, fixture_with, make_wc, plant_dead_app_lock, plant_lock_row,
    plant_work_item, standard_repo,
};
use crate::engine::{EngineFacade, EngineResult, OpContext};

/// Hooks that succeed and count how often they ran.
#[derive(Default)]
struct Finishing {
    queues: AtomicU32,
    finalized: AtomicU32,
}

#[async_trait]
impl RecoveryHooks for Finishing {
    async fn run_work_queue(&self, db: &WcDb, _ctx: &OpContext) -> EngineResult<()> {
        self.queues.fetch_add(1, Ordering::SeqCst);
        db.with_write(|tx| {
            tx.execute("DELETE FROM work_queue", [])
                .map(|_| ())
                .map_err(svn_wc::db::sql_err)
        })
    }

    async fn finalize_commit(
        &self,
        _db: &WcDb,
        record: &JournalRecord,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        assert!(matches!(record.phase, JournalPhase::Merged { .. }));
        self.finalized.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
async fn unfinished_work_without_a_runner_is_reported_for_cleanup() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    plant_work_item(&root);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(count_rows(&root, "work_queue"), 1, "left for cleanup");
}

#[tokio::test]
async fn a_leaked_server_transaction_is_deleted_from_the_journal() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let journal = f.engine.journals.begin(&root, URL).unwrap();
    // The app dies after the transaction exists and before MERGE.
    let editor = f
        .repo
        .commit_editor(
            "lost",
            &[],
            &[],
            false,
            Arc::new(journal),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    drop(editor);
    assert_eq!(f.repo.open_txns().len(), 1);
    let record = f.engine.journals.load(&root).unwrap().unwrap();
    assert!(matches!(record.phase, JournalPhase::TxnCreated { .. }));

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.recovered.is_empty() && opened.recovery.needs_user_action.is_empty());
    assert!(f.repo.open_txns().is_empty(), "transaction deleted");
    assert!(
        f.engine.journals.load(&root).unwrap().is_none(),
        "journal removed"
    );
}

/// Runs a commit of one new file up to MERGE and "crashes" before the working copy is updated.
async fn crash_after_merge(f: &super::support::Fixture, root: &Path) {
    let journal = f.engine.journals.begin(root, URL).unwrap();
    let mut editor = f
        .repo
        .commit_editor(
            "merged",
            &[],
            &[],
            false,
            Arc::new(journal),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let head = f.repo.youngest_rev(&CancelToken::new()).await.unwrap();
    let root_dir = editor.open_root(Some(head)).await.unwrap();
    let dir = editor
        .add_directory(&RelPath::new("extra").unwrap(), root_dir, None)
        .await
        .unwrap();
    editor.close_directory(dir).await.unwrap();
    editor.close_directory(root_dir).await.unwrap();
    editor.close_edit().await.unwrap();
}

#[tokio::test]
async fn a_merged_commit_is_finalized_not_aborted() {
    let hooks = Arc::new(Finishing::default());
    let f = fixture_with(hooks.clone());
    let (_tmp, root) = make_wc(1);
    crash_after_merge(&f, &root).await;
    let record = f.engine.journals.load(&root).unwrap().unwrap();
    assert_eq!(record.phase, JournalPhase::Merged { rev: 2 });

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert_eq!(hooks.finalized.load(Ordering::SeqCst), 1);
    assert!(f.engine.journals.load(&root).unwrap().is_none());
    // The revision stays: recovery never undoes a merged commit.
    assert_eq!(f.repo.youngest_rev(&CancelToken::new()).await.unwrap().0, 2);
}

#[tokio::test]
async fn a_merged_commit_that_cannot_be_finalized_is_kept_and_reported() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    crash_after_merge(&f, &root).await;
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(
        f.engine.journals.load(&root).unwrap().is_some(),
        "journal kept"
    );
}

#[tokio::test]
async fn journals_are_per_working_copy_and_do_not_overlap() {
    let f = fixture();
    let (_a, root_a) = make_wc(1);
    let (_b, root_b) = make_wc(1);
    let first = f.engine.journals.begin(&root_a, URL).unwrap();
    let second = f.engine.journals.begin(&root_b, URL).unwrap();
    let blocked = f.engine.journals.begin(&root_a, URL).unwrap_err();
    assert_eq!(blocked.code, ErrorCode::WcNeedsCleanup);
    first.complete().unwrap();
    assert!(f.engine.journals.load(&root_a).unwrap().is_none());
    assert!(f.engine.journals.load(&root_b).unwrap().is_some());
    second.complete().unwrap();
}

#[tokio::test]
async fn a_corrupt_journal_is_set_aside_and_does_not_block_opening() {
    let f = fixture();
    let (_tmp, root) = make_wc(1);
    let journal = f.engine.journals.begin(&root, URL).unwrap();
    // Damage the file the journal wrote.
    let dir = f.state.path().join("commit-journal");
    let file = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&file, b"{ not json").unwrap();
    let err = JournalStore::new(f.state.path().to_path_buf())
        .load(&root)
        .unwrap_err();
    assert!(matches!(err, JournalLoadError::Corrupt(_)), "{err:?}");
    assert_eq!(err.error().code, ErrorCode::Corrupt);

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(
        opened.recovery.needs_user_action,
        vec![String::new()],
        "the interrupted commit is reported, not silently forgotten"
    );
    assert!(
        !file.exists(),
        "the damaged journal no longer blocks commits"
    );
    let aside = file.with_extension("json.corrupt");
    assert_eq!(
        std::fs::read(&aside).unwrap(),
        b"{ not json",
        "kept for diagnosis"
    );
    f.engine
        .journals
        .begin(&root, URL)
        .unwrap()
        .complete()
        .unwrap();
    drop(journal);
}

#[cfg(unix)]
#[tokio::test]
async fn journals_are_keyed_by_the_canonical_root() {
    let f = fixture();
    let (tmp, root) = make_wc(1);
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    let journal = f.engine.journals.begin(&link, URL).unwrap();
    let record = f
        .engine
        .journals
        .load(&root)
        .unwrap()
        .expect("same journal");
    let canonical = std::fs::canonicalize(&root).unwrap();
    assert_eq!(record.wc_root, canonical.to_string_lossy());
    let blocked = f.engine.journals.begin(&root, URL).unwrap_err();
    assert_eq!(blocked.code, ErrorCode::WcNeedsCleanup);
    journal.complete().unwrap();
}

/// Fixture whose sessions fail `abort_txn` with `abort_error`, with a journal of a commit that
/// died after creating transaction `txn-1`.
fn txn_fixture(
    abort_error: Option<ErrorCode>,
) -> (
    super::support::Fixture,
    Arc<ScriptedSession>,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let repo = standard_repo();
    let session = Arc::new(ScriptedSession::new(repo.clone(), abort_error));
    let f = fixture_on(
        repo,
        Arc::new(ScriptedConnector(session.clone())),
        Arc::new(UnwiredRecovery),
    );
    let (tmp, root) = make_wc(1);
    let journal = f.engine.journals.begin(&root, URL).unwrap();
    journal.on_txn_created("txn-1").unwrap();
    (f, session, tmp, root)
}

#[tokio::test]
async fn a_transaction_the_server_no_longer_knows_clears_the_journal() {
    let (f, session, _tmp, root) = txn_fixture(Some(ErrorCode::PathNotFound));
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(
        f.engine.journals.load(&root).unwrap().is_none(),
        "a lost transaction must not block commits forever"
    );
    assert_eq!(
        session.calls(),
        vec!["auth", "abort"],
        "signed in before the abort"
    );
    f.engine
        .journals
        .begin(&root, URL)
        .unwrap()
        .complete()
        .unwrap();
}

#[tokio::test]
async fn a_temporary_abort_failure_keeps_the_journal_for_the_next_open() {
    let (f, session, _tmp, root) = txn_fixture(Some(ErrorCode::Network));
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(opened.recovery.needs_user_action.is_empty());
    assert!(
        f.engine.journals.load(&root).unwrap().is_some(),
        "retried later"
    );
    assert_eq!(session.calls(), vec!["auth", "abort"]);
}

#[tokio::test]
async fn a_merged_commit_is_not_finalized_while_a_foreign_lock_is_held() {
    let hooks = Arc::new(Finishing::default());
    let f = fixture_with(hooks.clone());
    let (_tmp, root) = make_wc(1);
    crash_after_merge(&f, &root).await;
    plant_lock_row(&root, "");

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(
        hooks.finalized.load(Ordering::SeqCst),
        0,
        "never under a foreign lock"
    );
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(
        f.engine.journals.load(&root).unwrap().is_some(),
        "journal kept"
    );

    // Once the other program is done, the next open finalizes.
    let db = WcDb::open(&root).unwrap();
    db.with_write(|tx| {
        tx.execute("DELETE FROM wc_lock", [])
            .map(|_| ())
            .map_err(svn_wc::db::sql_err)
    })
    .unwrap();
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert_eq!(hooks.finalized.load(Ordering::SeqCst), 1);
    assert!(f.engine.journals.load(&root).unwrap().is_none());
}

/// Hooks whose work queue run fails; counts finalizations.
#[derive(Default)]
struct StuckQueue {
    finalized: AtomicU32,
}

#[async_trait]
impl RecoveryHooks for StuckQueue {
    async fn run_work_queue(&self, _db: &WcDb, _ctx: &OpContext) -> EngineResult<()> {
        Err(svn_types::SvnError::new(ErrorCode::Internal, "stuck"))
    }

    async fn finalize_commit(
        &self,
        _db: &WcDb,
        _record: &JournalRecord,
        _ctx: &OpContext,
    ) -> EngineResult<()> {
        self.finalized.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn a_merged_commit_is_not_finalized_over_unfinished_work() {
    let hooks = Arc::new(StuckQueue::default());
    let f = fixture_with(hooks.clone());
    let (_tmp, root) = make_wc(1);
    crash_after_merge(&f, &root).await;
    plant_work_item(&root);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(hooks.finalized.load(Ordering::SeqCst), 0);
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(f.engine.journals.load(&root).unwrap().is_some());
}

/// Path of the only journal file of the fixture's state directory.
fn journal_file(f: &super::support::Fixture) -> std::path::PathBuf {
    let dir = f.state.path().join("commit-journal");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files.pop().unwrap()
}

/// Names below the journal directory, sorted.
fn journal_dir_listing(f: &super::support::Fixture) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(f.state.path().join("commit-journal"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Fixture with finishing hooks and a journal file of `root` holding `contents`.
fn fixture_with_journal(
    contents: &[u8],
) -> (
    super::support::Fixture,
    Arc<Finishing>,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let hooks = Arc::new(Finishing::default());
    let f = fixture_with(hooks.clone());
    let (tmp, root) = make_wc(1);
    drop(f.engine.journals.begin(&root, URL).unwrap());
    let file = journal_file(&f);
    std::fs::write(&file, contents).unwrap();
    (f, hooks, tmp, root, file)
}

#[tokio::test]
async fn a_current_format_journal_that_does_not_parse_is_set_aside() {
    let (f, hooks, _tmp, root, file) = fixture_with_journal(br#"{"version":1,"phase":"exploded"}"#);
    let err = f.engine.journals.load(&root).unwrap_err();
    assert!(matches!(err, JournalLoadError::Corrupt(_)), "{err:?}");

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert!(!file.exists());
    assert_eq!(
        std::fs::read(file.with_extension("json.corrupt")).unwrap(),
        br#"{"version":1,"phase":"exploded"}"#
    );
    assert_eq!(hooks.finalized.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_journal_of_an_unknown_format_is_kept_untouched_and_not_finalized() {
    let contents = format!(
        r#"{{"version":2,"wc_root":"x","repo_root_url":"{URL}","phase":"merged","rev":2,"extra":true}}"#
    );
    let (f, hooks, _tmp, root, file) = fixture_with_journal(contents.as_bytes());
    match f.engine.journals.load(&root).unwrap_err() {
        JournalLoadError::UnsupportedVersion { version, .. } => assert_eq!(version, 2),
        other => panic!("expected an unsupported version, got {other:?}"),
    }
    let before = journal_dir_listing(&f);

    for _ in 0..2 {
        let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
        assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
        assert!(opened.recovery.recovered.is_empty());
    }
    assert_eq!(hooks.finalized.load(Ordering::SeqCst), 0, "never finalized");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        contents.as_bytes(),
        "untouched"
    );
    assert_eq!(journal_dir_listing(&f), before, "not moved aside");

    let err = f.engine.journals.begin(&root, URL).unwrap_err();
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup);
    assert!(
        err.message.contains("journal format 2"),
        "the refusal explains itself: {err}"
    );
}

#[tokio::test]
async fn a_journal_that_cannot_be_read_is_kept_for_the_next_open() {
    let (f, hooks, _tmp, root, file) = fixture_with_journal(b"");
    // Something that exists but cannot be read as a file: a directory in its place.
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    std::fs::write(file.join("inside"), b"keep").unwrap();
    let err = f.engine.journals.load(&root).unwrap_err();
    assert!(matches!(err, JournalLoadError::Unreadable(_)), "{err:?}");
    let before = journal_dir_listing(&f);

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(journal_dir_listing(&f), before, "nothing moved or removed");
    assert_eq!(std::fs::read(file.join("inside")).unwrap(), b"keep");

    // Once readable again, the next open recovers it as usual.
    std::fs::remove_dir_all(&file).unwrap();
    let journal = f.engine.journals.begin(&root, URL).unwrap();
    journal
        .on_merged(&svn_ra_api::CommitInfo {
            revision: svn_types::Revnum(2),
            date: None,
            author: None,
            post_commit_err: None,
        })
        .unwrap();
    drop(journal);
    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.recovered, vec![String::new()]);
    assert_eq!(hooks.finalized.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_earlier_quarantined_journal_is_never_replaced() {
    let (f, _hooks, _tmp, root, file) = fixture_with_journal(b"garbage");
    std::fs::write(file.with_extension("json.corrupt"), b"older").unwrap();

    f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert!(!file.exists());
    assert_eq!(
        std::fs::read(file.with_extension("json.corrupt")).unwrap(),
        b"older"
    );
    assert_eq!(
        std::fs::read(file.with_extension("json.corrupt.1")).unwrap(),
        b"garbage"
    );
}

#[tokio::test]
async fn a_corrupt_journal_that_cannot_be_set_aside_stays_in_place() {
    let (f, _hooks, _tmp, root, file) = fixture_with_journal(b"garbage");
    // Every quarantine slot is taken: setting it aside fails.
    std::fs::write(file.with_extension("json.corrupt"), b"old").unwrap();
    for n in 1..8 {
        std::fs::write(file.with_extension(format!("json.corrupt.{n}")), b"old").unwrap();
    }
    let before = journal_dir_listing(&f);

    let opened = f.engine.open_wc(&root, &ctx()).await.unwrap();
    assert_eq!(opened.recovery.needs_user_action, vec![String::new()]);
    assert_eq!(std::fs::read(&file).unwrap(), b"garbage", "never deleted");
    assert_eq!(journal_dir_listing(&f), before);
    assert_eq!(
        f.engine.journals.begin(&root, URL).unwrap_err().code,
        ErrorCode::WcNeedsCleanup,
        "commits stay blocked until the user looks"
    );
}
