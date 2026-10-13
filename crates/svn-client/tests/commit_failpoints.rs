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
