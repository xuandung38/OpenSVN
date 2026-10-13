//! Crash simulation around the server merge. Run with `--features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use fail::FailScenario;
use support::rec_ra::RecRa;
use support::*;
use svn_client::commit::journal::JournalStore;
use svn_client::commit::{CommitContext, CommitOptions, commit, recover_commits};
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{CancelToken, ErrorCode, Result};
use svn_wc::WcDb;

struct Env {
    fx: Fixture,
    wc: PathBuf,
    db: Arc<WcDb>,
    ra: RecRa,
    state: PathBuf,
    cancel: CancelToken,
}

fn env() -> Option<Env> {
    let fx = fixture()?;
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    let fake = FakeRaSession::new("https://svn.test.local/repo", "uuid-1").expect("fake");
    let put = |p: &str, t: &str| FakeChange::PutFile(rp(p), Bytes::from(t.to_owned()));
    fake.seed(
        "tester",
        "seed",
        &[
            put("d/f1", "1\n"),
            put("d/f2", "2\n"),
            put("d/sub/g", "3\n"),
            put("top.txt", "t\n"),
            FakeChange::PutDir(rp("e")),
        ],
    )
    .expect("seed");
    let db = Arc::new(open(&wc));
    let state = fx.base.join("state");
    Some(Env {
        fx,
        wc,
        db,
        ra: RecRa::new(fake),
        state,
        cancel: CancelToken::new(),
    })
}

impl Env {
    async fn commit(&self) -> Result<Option<svn_client::commit::CommitResult>> {
        commit(
            &self.db,
            &CommitContext {
                ra: &self.ra,
                state_dir: &self.state,
                cancel: &self.cancel,
            },
            std::slice::from_ref(&self.wc),
            &CommitOptions::new("m"),
        )
        .await
    }

    async fn recover(&self) -> svn_client::commit::RecoveryReport {
        let wc = self.wc.clone();
        let open_wc = move |p: &Path| -> Result<WcDb> {
            assert_eq!(p, wc);
            Ok(open(p))
        };
        recover_commits(&self.state, &self.ra, &self.cancel, &open_wc)
            .await
            .expect("recover")
    }
}

#[tokio::test]
async fn crash_after_merge_is_finalized_on_recovery() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    write(&e.wc, "new.txt", "new\n");
    local_add(&e, "new.txt");
    let before = dump_rows(&e.wc);

    fail::cfg("commit.after_merge_before_finalize", "return").expect("cfg");
    let err = e.commit().await.expect_err("crash");
    assert_eq!(err.code, ErrorCode::Internal);
    fail::remove("commit.after_merge_before_finalize");

    // Server has the revision, the working copy has not seen it yet.
    assert_eq!(e.ra.inner.youngest_rev(&e.cancel).await.expect("rev").0, 2);
    assert_eq!(dump_rows(&e.wc), before, "WC untouched before finalize");
    assert!(svn_status(&e.fx.oracle, &e.wc).contains("M       d/f1"));
    let journals = JournalStore::new(&e.state).list().expect("list");
    assert_eq!(journals.len(), 1);
    let data = JournalStore::read(&journals[0]).expect("read");
    assert_eq!(data.merged.as_ref().map(|m| m.revision), Some(2));

    let report = e.recover().await;
    assert_eq!(report.finalized.len(), 1, "{report:?}");
    assert!(report.failed.is_empty());
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "", "no phantom 'A' or 'M'");
    assert!(JournalStore::new(&e.state).list().expect("list").is_empty());
    assert!(e.db.work_items().expect("wq").is_empty());
    // Nothing left to recover.
    let again = e.recover().await;
    assert!(again.finalized.is_empty() && again.aborted.is_empty());
    svn(&e.fx.oracle, &e.wc, &["cleanup"]);
}

fn local_add(e: &Env, rel: &str) {
    svn_client::local::add(
        &e.db,
        &e.cancel,
        &[e.wc.join(rel)],
        &svn_client::local::AddOptions::default(),
        &svn_client::local::AddConfig::default(),
    )
    .expect("add");
}

#[tokio::test]
async fn finalize_is_idempotent_after_a_crash_inside_it() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    write(&e.wc, "new.txt", "new\n");
    local_add(&e, "new.txt");
    local_delete(&e, "top.txt");
    fail::cfg("commit.after_merge_before_finalize", "return").expect("cfg");
    e.commit().await.expect_err("crash");
    fail::remove("commit.after_merge_before_finalize");

    // The post-commit transaction ran, but the process died before the journal was removed.
    let store = JournalStore::new(&e.state);
    let path = store.list().expect("list").remove(0);
    let data = JournalStore::read(&path).expect("read");
    svn_client::local::lock::with_wc_lock(&e.db, &e.cancel, || {
        svn_client::commit::finalize::finalize(&e.db, &data, &store.data_dir_of(&data.id))
    })
    .expect("first finalize");
    let after_first = dump_rows(&e.wc);

    let report = e.recover().await;
    assert_eq!(report.finalized.len(), 1, "{report:?}");
    assert_eq!(
        dump_rows(&e.wc),
        after_first,
        "second finalize changes nothing"
    );
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

fn local_delete(e: &Env, rel: &str) {
    svn_client::local::delete(
        &e.db,
        &e.cancel,
        &[e.wc.join(rel)],
        &svn_client::local::DeleteOptions::default(),
    )
    .expect("rm");
}

#[tokio::test]
async fn failure_after_txn_creation_cleans_up_the_server() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    let before = dump_rows(&e.wc);
    fail::cfg("commit.after_txn_created", "return").expect("cfg");
    e.commit().await.expect_err("injected");
    fail::remove("commit.after_txn_created");
    assert!(e.ra.inner.open_txns().is_empty());
    assert_eq!(dump_rows(&e.wc), before);
    assert!(JournalStore::new(&e.state).list().expect("list").is_empty());
    assert_eq!(e.ra.inner.youngest_rev(&e.cancel).await.expect("rev").0, 1);
}

/// Commits a keyword edit and dies after the post-commit transaction, before the `file-commit`
/// items ran: the rows already say r2, the working file still has the old keyword values.
async fn crash_before_post_commit_queue(e: &Env) {
    write(&e.wc, "d/f1", "rev $Rev$\n");
    svn_client::local::propset(
        &e.db,
        &e.cancel,
        &rp("d/f1"),
        "svn:keywords",
        Some(b"Rev"),
        svn_client::local::PropSetOptions::default(),
    )
    .expect("propset");
    fail::cfg("commit.after_finalize_before_queue", "return").expect("cfg");
    e.commit().await.expect_err("crash");
    fail::remove("commit.after_finalize_before_queue");

    let node = e.db.read_node(&rp("d/f1")).expect("read").expect("node");
    assert_eq!(node.revision, Some(2), "rows were committed");
    let items = e.db.work_items().expect("wq");
    assert_eq!(items.len(), 1, "one file-commit item queued: {items:?}");
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev$\n", "work item not run yet");
    assert_eq!(JournalStore::new(&e.state).list().expect("list").len(), 1);
}

#[tokio::test]
async fn post_commit_work_left_by_a_crash_is_finished_by_cleanup() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    crash_before_post_commit_queue(&e).await;

    let report = svn_client::local::cleanup(&e.db, &e.cancel).expect("cleanup");
    assert_eq!(report.work_items, 1);
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 2 $\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");

    // The journal still finalizes cleanly afterwards (idempotent) and goes away.
    let report = e.recover().await;
    assert_eq!(report.finalized.len(), 1, "{report:?}");
    assert!(e.db.work_items().expect("wq").is_empty());
    assert!(JournalStore::new(&e.state).list().expect("list").is_empty());
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 2 $\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

#[tokio::test]
async fn post_commit_work_left_by_a_crash_is_finished_by_svn_cleanup() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    crash_before_post_commit_queue(&e).await;

    // The queued item has svn's format: svn 1.15 itself completes it.
    svn(&e.fx.oracle, &e.wc, &["cleanup"]);
    assert!(e.db.work_items().expect("wq").is_empty());
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 2 $\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");

    let report = e.recover().await;
    assert_eq!(report.finalized.len(), 1, "{report:?}");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

#[tokio::test]
async fn post_commit_work_left_by_a_crash_is_finished_by_the_next_operation() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    crash_before_post_commit_queue(&e).await;

    // Any write operation finishes leftover work before its own.
    write(&e.wc, "later.txt", "later\n");
    local_add(&e, "later.txt");
    assert!(e.db.work_items().expect("wq").is_empty());
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 2 $\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "A       later.txt\n");
}

#[tokio::test]
async fn post_commit_work_keeps_a_file_changed_after_the_commit_modified() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    crash_before_post_commit_queue(&e).await;

    // Edited after the commit read it: the fixup must not cache its stat as "unmodified".
    write(&e.wc, "d/f1", "rev $Rev$\nedited later\n");
    svn_client::local::cleanup(&e.db, &e.cancel).expect("cleanup");
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 2 $\nedited later\n");
    let node = e.db.read_node(&rp("d/f1")).expect("read").expect("node");
    assert_eq!(
        node.translated_size, None,
        "no stat cache for a modified file"
    );
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "M       d/f1\n");
}

/// A merged commit left behind by a failure after MERGE is folded in before the next commit on
/// the same working copy, so that commit starts from the true BASE.
#[tokio::test]
async fn next_commit_finalizes_a_pending_merged_journal_first() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    fail::cfg("commit.after_merge_before_finalize", "return").expect("cfg");
    e.commit().await.expect_err("crash");
    fail::remove("commit.after_merge_before_finalize");

    write(&e.wc, "d/f2", "other\n");
    let res = e.commit().await.expect("commit").expect("some");
    assert_eq!(res.revision.0, 3);
    assert_eq!(
        res.committed,
        vec![rp("d/f2")],
        "d/f1 came from the journal"
    );
    assert!(JournalStore::new(&e.state).list().expect("list").is_empty());
    let f1 = e.db.read_base(&rp("d/f1")).expect("read").expect("base");
    assert_eq!(f1.revision, Some(2));
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

/// A stale journal replayed after the path was committed again must not take BASE back to the
/// older revision and text.
#[tokio::test]
async fn stale_journal_never_moves_base_backwards() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "rev $Rev$\nfirst\n");
    svn_client::local::propset(
        &e.db,
        &e.cancel,
        &rp("d/f1"),
        "svn:keywords",
        Some(b"Rev"),
        svn_client::local::PropSetOptions::default(),
    )
    .expect("propset");
    fail::cfg("commit.after_merge_before_finalize", "return").expect("cfg");
    e.commit().await.expect_err("crash");
    fail::remove("commit.after_merge_before_finalize");

    // Keep a copy of the r2 journal and its data, as a process that died mid-removal would.
    let store = JournalStore::new(&e.state);
    let path = store.list().expect("list").remove(0);
    let data = JournalStore::read(&path).expect("read");
    let saved = e.fx.base.join("saved");
    std::fs::create_dir_all(&saved).expect("mkdir");
    std::fs::copy(&path, saved.join("journal.json")).expect("copy");
    let data_dir = store.data_dir_of(&data.id);
    let saved_data = saved.join("data");
    std::fs::create_dir_all(&saved_data).expect("mkdir");
    for f in std::fs::read_dir(&data_dir).expect("rd").flatten() {
        std::fs::copy(f.path(), saved_data.join(f.file_name())).expect("copy");
    }
    assert_eq!(e.recover().await.finalized.len(), 1);

    // The same file moves on to r3.
    write(&e.wc, "d/f1", "rev $Rev: 2 $\nsecond\n");
    assert_eq!(
        e.commit().await.expect("commit").expect("some").revision.0,
        3
    );
    let r3 = e.db.read_base(&rp("d/f1")).expect("read").expect("base");
    assert_eq!(r3.revision, Some(3));
    let rows_r3 = dump_rows(&e.wc);

    // The stale r2 journal reappears and is recovered.
    std::fs::copy(saved.join("journal.json"), &path).expect("restore");
    std::fs::create_dir_all(&data_dir).expect("mkdir");
    for f in std::fs::read_dir(&saved_data).expect("rd").flatten() {
        std::fs::copy(f.path(), data_dir.join(f.file_name())).expect("copy");
    }
    let report = e.recover().await;
    assert_eq!(report.finalized.len(), 1, "{report:?}");
    let after = e.db.read_base(&rp("d/f1")).expect("read").expect("base");
    assert_eq!(after.revision, Some(3), "BASE stays at r3");
    assert_eq!(after.checksum, r3.checksum, "BASE text stays r3");
    assert_eq!(dump_rows(&e.wc), rows_r3, "nothing changed");
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 3 $\nsecond\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

// ---- MERGE sent, outcome unknown (#85) ----

use svn_client::commit::{WcCommitRecovery, recover_wc_commits};
use svn_ra_api::fake::FakeMergeFault;

impl Env {
    async fn recover_wc(&self, with_session: bool) -> WcCommitRecovery {
        let ra: Option<&dyn RaSession> = if with_session { Some(&self.ra) } else { None };
        recover_wc_commits(&self.state, &self.db, ra, &self.cancel)
            .await
            .expect("recover")
    }

    fn only_journal(&self) -> svn_client::commit::journal::JournalData {
        let files = JournalStore::new(&self.state).list().expect("list");
        assert_eq!(files.len(), 1, "exactly one journal: {files:?}");
        JournalStore::read(&files[0]).expect("read")
    }

    fn assert_no_journal(&self) {
        assert!(
            JournalStore::new(&self.state)
                .list()
                .expect("list")
                .is_empty(),
            "no journal left"
        );
    }

    async fn youngest(&self) -> u64 {
        self.ra
            .inner
            .youngest_rev(&self.cancel)
            .await
            .expect("rev")
            .0
    }

    /// The next commit of this working copy is refused while a journal has an unknown outcome.
    async fn assert_next_commit_refused(&self) {
        write(&self.wc, "top.txt", "blocked?\n");
        let err = self.commit().await.expect_err("refused");
        assert_eq!(err.code, ErrorCode::WcNeedsCleanup, "{err}");
        write(&self.wc, "top.txt", "t\n");
    }
}

/// The process dies after the MERGE was sent and applied, before the revision reached the
/// journal: the journal only says "MERGE sent". Recovery finds the transaction gone and the
/// commit in the log, and finalizes it.
#[tokio::test]
async fn crash_after_merge_sent_is_found_in_the_log_and_finalized() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    write(&e.wc, "new.txt", "new\n");
    local_add(&e, "new.txt");
    let before = dump_rows(&e.wc);
    fail::cfg("commit.journal_merged_write", "return").expect("cfg");
    fail::cfg("commit.after_merge_before_finalize", "return").expect("cfg");
    e.commit().await.expect_err("crash");
    fail::remove("commit.after_merge_before_finalize");
    fail::remove("commit.journal_merged_write");

    assert_eq!(e.youngest().await, 2, "the server has the revision");
    let j = e.only_journal();
    assert!(j.merge_sent && j.merged.is_none(), "{j:?}");
    assert_eq!(j.start_youngest, Some(1));
    assert_eq!(j.message.as_deref(), Some("m"));
    assert_eq!(dump_rows(&e.wc), before);
    e.assert_next_commit_refused().await;
    assert_eq!(e.youngest().await, 2, "nothing committed twice");

    let report = e.recover_wc(true).await;
    assert_eq!(report.finalized, vec![j.id.clone()], "{report:?}");
    assert!(report.needs_user.is_empty() && report.failed.is_empty());
    e.assert_no_journal();
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
    let f1 = e.db.read_base(&rp("d/f1")).expect("read").expect("base");
    assert_eq!(f1.revision, Some(2));
    svn(&e.fx.oracle, &e.wc, &["cleanup"]);
}

/// `CommitOutcomeUnknown` while the transaction still exists (the server never applied the
/// MERGE): the commit keeps the journal and the transaction; recovery deletes the transaction,
/// which proves nothing was merged, and drops the journal.
#[tokio::test]
async fn outcome_unknown_with_the_transaction_present_is_aborted() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    let before = dump_rows(&e.wc);
    e.ra.inner.fail_next_merge(FakeMergeFault::LostBeforeMerge);
    let err = e.commit().await.expect_err("unknown");
    assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown, "{err}");
    assert!(
        !e.ra.events().iter().any(|l| l.starts_with("abort")),
        "never aborted: {:?}",
        e.ra.events()
    );
    assert_eq!(e.ra.inner.open_txns().len(), 1, "transaction kept");
    let j = e.only_journal();
    assert!(j.outcome_unknown());
    assert_eq!(dump_rows(&e.wc), before, "WC untouched");
    e.assert_next_commit_refused().await;

    // Without a session nothing can be decided: the journal stays.
    let offline = e.recover_wc(false).await;
    assert_eq!(offline.failed.len(), 1, "{offline:?}");
    assert_eq!(e.only_journal().id, j.id);

    let report = e.recover_wc(true).await;
    assert_eq!(report.aborted, vec![j.id], "{report:?}");
    assert!(e.ra.inner.open_txns().is_empty());
    e.assert_no_journal();
    assert_eq!(dump_rows(&e.wc), before);
    assert_eq!(e.youngest().await, 1);
    // The working copy commits normally again.
    let res = e.commit().await.expect("commit").expect("some");
    assert_eq!(res.revision.0, 2);
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

/// `CommitOutcomeUnknown` after the server created the revision: the transaction is gone, the
/// log shows the commit, recovery finalizes it.
#[tokio::test]
async fn outcome_unknown_with_the_transaction_gone_is_finalized() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    local_delete(&e, "top.txt");
    e.ra.inner.fail_next_merge(FakeMergeFault::LostAfterMerge);
    let err = e.commit().await.expect_err("unknown");
    assert_eq!(err.code, ErrorCode::CommitOutcomeUnknown, "{err}");
    assert!(e.ra.inner.open_txns().is_empty());
    assert_eq!(e.youngest().await, 2);
    let j = e.only_journal();
    assert!(j.outcome_unknown());

    let report = e.recover_wc(true).await;
    assert_eq!(report.finalized, vec![j.id], "{report:?}");
    e.assert_no_journal();
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
    assert!(!e.wc.join("top.txt").exists());
    // The global recovery has nothing left either.
    let again = e.recover().await;
    assert!(again.finalized.is_empty() && again.aborted.is_empty() && again.needs_user.is_empty());
}

/// The transaction is gone but no revision matches the commit (someone removed the transaction
/// and committed something else): the journal is kept and reported for the user, never dropped.
#[tokio::test]
async fn outcome_unknown_without_a_matching_revision_needs_the_user() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    e.ra.inner.fail_next_merge(FakeMergeFault::LostBeforeMerge);
    e.commit().await.expect_err("unknown");
    let j = e.only_journal();
    let txn = j.txn.clone().expect("txn");
    e.ra.inner.abort_txn(&txn).await.expect("admin removes it");
    e.ra.inner
        .seed(
            "other",
            "m",
            &[FakeChange::PutFile(
                rp("d/f1"),
                Bytes::from_static(b"other\n"),
            )],
        )
        .expect("seed");

    let report = e.recover_wc(true).await;
    assert_eq!(report.needs_user.len(), 1, "{report:?}");
    assert_eq!(report.needs_user[0].0, j.id);
    assert_eq!(report.needs_user[0].1.code, ErrorCode::WcNeedsCleanup);
    assert_eq!(e.only_journal(), j, "journal untouched");
    e.assert_next_commit_refused().await;
    // The global recovery reports it the same way.
    let global = e.recover().await;
    assert_eq!(global.needs_user.len(), 1, "{global:?}");
    assert_eq!(e.only_journal(), j);
}

/// #85 item 5: the journal write after MERGE fails, then updating the working copy fails. The
/// error says recovery finishes the commit, and it does (from the log, as the journal never
/// got the revision).
#[tokio::test]
async fn double_failure_after_merge_is_finished_by_recovery() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    write(&e.wc, "new.txt", "new\n");
    local_add(&e, "new.txt");
    fail::cfg("commit.journal_merged_write", "return").expect("cfg");
    fail::cfg("commit.finalize", "return").expect("cfg");
    let err = e.commit().await.expect_err("double failure");
    fail::remove("commit.finalize");
    fail::remove("commit.journal_merged_write");
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup, "{err}");
    assert!(err.message.contains("committed revision 2"), "{err}");
    assert!(err.message.contains("recovery"), "{err}");
    let j = e.only_journal();
    assert!(
        j.outcome_unknown(),
        "the revision never reached the journal"
    );
    e.assert_next_commit_refused().await;

    let report = e.recover_wc(true).await;
    assert_eq!(report.finalized, vec![j.id], "{report:?}");
    e.assert_no_journal();
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
    assert_eq!(e.youngest().await, 2);
}

/// Same double failure, but the retry of the journal write succeeds: the journal records the
/// revision and the next commit finalizes it first.
#[tokio::test]
async fn double_failure_with_a_recorded_revision_is_finished_by_the_next_commit() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    fail::cfg("commit.journal_merged_write", "1*return").expect("cfg");
    fail::cfg("commit.finalize", "return").expect("cfg");
    let err = e.commit().await.expect_err("double failure");
    fail::remove("commit.finalize");
    fail::remove("commit.journal_merged_write");
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup, "{err}");
    assert_eq!(
        e.only_journal().merged.map(|m| m.revision),
        Some(2),
        "retried write"
    );
    write(&e.wc, "d/f2", "other\n");
    let res = e.commit().await.expect("commit").expect("some");
    assert_eq!(res.revision.0, 3);
    e.assert_no_journal();
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

/// The marker is on disk, then the process fails before the MERGE goes out: the transaction is
/// deleted, the repository has no new revision, and the journal goes.
#[tokio::test]
async fn failure_between_marker_and_merge_drops_the_journal() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    let before = dump_rows(&e.wc);
    fail::cfg("commit.after_merge_sent_marker", "return").expect("cfg");
    e.commit().await.expect_err("injected");
    fail::remove("commit.after_merge_sent_marker");
    assert!(e.ra.inner.open_txns().is_empty());
    assert_eq!(e.youngest().await, 1);
    e.assert_no_journal();
    assert_eq!(dump_rows(&e.wc), before);
}

/// The server refuses the MERGE (out of date: another commit landed during the upload). The
/// journal said "MERGE sent", so the log is checked before it goes; another user's commit with
/// the same message and paths but different text is not mistaken for ours.
#[tokio::test]
async fn merge_refused_as_out_of_date_drops_the_journal_after_checking_the_log() {
    let _scenario = FailScenario::setup();
    let Some(mut e) = env() else { return };
    write(&e.wc, "d/f1", "mine\n");
    e.ra.seed_on_text = Some((
        "tester".into(),
        "m".into(),
        vec![FakeChange::PutFile(
            rp("d/f1"),
            Bytes::from_static(b"theirs\n"),
        )],
    ));
    let before = dump_rows(&e.wc);
    let err = e.commit().await.expect_err("out of date");
    assert_eq!(err.code, ErrorCode::OutOfDate, "{err}");
    assert!(
        e.ra.events()
            .iter()
            .any(|l| l.starts_with("abort_txn_checked")),
        "reconciled: {:?}",
        e.ra.events()
    );
    e.assert_no_journal();
    assert_eq!(dump_rows(&e.wc), before);
    assert_eq!(e.youngest().await, 2);
}

/// Writes a version 1 journal (no `merge_sent` marker) of this working copy naming `txn`.
fn plant_v1_journal(e: &Env, txn: &str) -> PathBuf {
    std::fs::create_dir_all(&e.state).expect("state dir");
    let path = e.state.join("commit-v1.json");
    let v1 = serde_json::json!({
        "version": 1,
        "id": "v1",
        "wcroot": e.db.wcroot(),
        "keep_locks": false,
        "keep_changelist": false,
        "txn": txn,
        "merged": null,
        "items": [],
    });
    std::fs::write(&path, serde_json::to_vec(&v1).expect("json")).expect("write");
    path
}

/// #92 review: a version 1 journal naming a transaction may have been left during the MERGE.
/// The transaction still there: deleted now, so the commit never happened and the journal goes.
#[tokio::test]
async fn a_v1_journal_whose_transaction_still_exists_is_aborted() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "changed\n");
    e.ra.inner.fail_next_merge(FakeMergeFault::LostBeforeMerge);
    e.commit().await.expect_err("unknown");
    let txn = e.only_journal().txn.expect("txn");
    // Replace it with the version 1 journal an older build would have left.
    for p in JournalStore::new(&e.state).list().expect("list") {
        std::fs::remove_file(p).expect("rm");
    }
    plant_v1_journal(&e, &txn);
    e.assert_next_commit_refused().await;

    let report = e.recover_wc(true).await;
    assert_eq!(report.aborted, vec!["v1".to_owned()], "{report:?}");
    assert!(e.ra.inner.open_txns().is_empty());
    e.assert_no_journal();
}

/// The transaction of a version 1 journal is gone: it may have been merged, and the journal
/// has no start revision or message to search the log with, so it is kept for the user.
#[tokio::test]
async fn a_v1_journal_whose_transaction_is_gone_needs_the_user() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    let path = plant_v1_journal(&e, "gone-txn");
    let before = std::fs::read(&path).expect("read");
    let report = e.recover_wc(true).await;
    assert_eq!(report.needs_user.len(), 1, "{report:?}");
    assert!(report.aborted.is_empty());
    assert_eq!(std::fs::read(&path).expect("kept"), before, "untouched");
    e.assert_next_commit_refused().await;
    assert_eq!(e.youngest().await, 1);
}

/// #92 review: a journal file that cannot be read may belong to this working copy and say the
/// MERGE was sent, so it blocks new commits until it is readable again.
#[tokio::test]
async fn an_unreadable_journal_blocks_new_commits() {
    let _scenario = FailScenario::setup();
    let Some(e) = env() else { return };
    // Something that lists as a journal but cannot be read as a file.
    let bad = e.state.join("commit-unreadable.json");
    std::fs::create_dir_all(&bad).expect("dir");
    write(&e.wc, "d/f1", "changed\n");
    let err = e.commit().await.expect_err("blocked");
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup, "{err}");
    assert!(err.message.contains("cannot be read"), "{err}");
    assert_eq!(e.youngest().await, 1, "nothing committed");
    assert!(bad.is_dir(), "kept");

    std::fs::remove_dir(&bad).expect("readable again");
    let res = e.commit().await.expect("commit").expect("some");
    assert_eq!(res.revision.0, 2);
}
