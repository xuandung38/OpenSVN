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
