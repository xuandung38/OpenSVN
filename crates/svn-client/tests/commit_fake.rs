//! Commit against the in-memory repository: the editor drive is recorded and asserted, and the
//! resulting working copy is validated by svn 1.15 itself.
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use support::rec_ra::{RecRa, raw_error};
use support::*;
use svn_client::commit::{CommitContext, CommitOptions, CommitResult, commit, map_commit_error};
use svn_client::local::{self, AddConfig, AddOptions, DeleteOptions};
use svn_ra_api::RaSession;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_types::{CancelToken, Depth, ErrorCode, PropChange, Result};
use svn_wc::WcDb;
use svn_wc::db::LockState;

struct Env {
    fx: Fixture,
    wc: PathBuf,
    db: Arc<WcDb>,
    ra: RecRa,
    state: PathBuf,
    cancel: CancelToken,
}

fn seed(fake: &FakeRaSession) {
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
            FakeChange::SetProp(
                rp("d/f1"),
                PropChange {
                    name: "k".into(),
                    value: Some(Bytes::from_static(b"v")),
                },
            ),
        ],
    )
    .expect("seed");
}

fn env() -> Option<Env> {
    let fx = fixture()?;
    let wc = checkout(&fx.oracle, &fx.base, &fx.url, "wc");
    let fake = FakeRaSession::new("https://svn.test.local/repo", "uuid-1").expect("fake");
    seed(&fake);
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
    async fn commit(
        &self,
        targets: &[PathBuf],
        opts: &CommitOptions,
    ) -> Result<Option<CommitResult>> {
        commit(
            &self.db,
            &CommitContext {
                ra: &self.ra,
                state_dir: &self.state,
                cancel: &self.cancel,
            },
            targets,
            opts,
        )
        .await
    }

    async fn commit_all(&self, msg: &str) -> Result<Option<CommitResult>> {
        self.commit(std::slice::from_ref(&self.wc), &CommitOptions::new(msg))
            .await
    }

    fn add(&self, rels: &[&str]) {
        let paths: Vec<PathBuf> = rels.iter().map(|r| self.wc.join(r)).collect();
        local::add(
            &self.db,
            &self.cancel,
            &paths,
            &AddOptions::default(),
            &AddConfig::default(),
        )
        .expect("add");
    }

    fn propset(&self, rel: &str, name: &str, value: &str) {
        local::propset(
            &self.db,
            &self.cancel,
            &rp(rel),
            name,
            Some(value.as_bytes()),
            local::PropSetOptions::default(),
        )
        .expect("propset");
    }

    async fn server_file(&self, path: &str) -> String {
        let mut buf: Vec<u8> = Vec::new();
        self.ra
            .inner
            .get_file(&rp(path), None, &mut buf, &self.cancel)
            .await
            .expect("get_file");
        String::from_utf8(buf).expect("utf8")
    }

    fn journal_files(&self) -> Vec<String> {
        std::fs::read_dir(&self.state)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn assert_clean_for_svn(&self) {
        assert_eq!(svn_status(&self.fx.oracle, &self.wc), "", "svn status");
        svn(&self.fx.oracle, &self.wc, &["cleanup"]);
    }
}

fn events_after_editor(ra: &RecRa) -> Vec<String> {
    let all = ra.events();
    let i = all
        .iter()
        .position(|e| e.starts_with("commit_editor"))
        .expect("commit_editor called");
    all[i + 1..].to_vec()
}

#[tokio::test]
async fn mixed_changes_are_driven_in_order_and_folded_into_the_wc() {
    let Some(mut e) = env() else { return };
    // The authentication hook must see the WC unlocked.
    let unlocked = Arc::new(AtomicBool::new(false));
    {
        let (db, flag) = (e.db.clone(), unlocked.clone());
        e.ra.on_auth = Some(Box::new(move || {
            let free = db.wc_lock_held(&rp("")).expect("held") == LockState::Free;
            flag.store(free, Ordering::SeqCst);
        }));
    }
    write(&e.wc, "d/f1", "changed\n");
    e.propset("d/f2", "p", "v");
    write(&e.wc, "n.txt", "new\n");
    write(&e.wc, "nd/x", "x\n");
    e.add(&["n.txt", "nd"]);
    local::delete(
        &e.db,
        &e.cancel,
        &[e.wc.join("top.txt")],
        &DeleteOptions::default(),
    )
    .expect("rm");

    let res = e
        .commit_all("mixed")
        .await
        .expect("commit")
        .expect("something");
    assert_eq!(res.revision.0, 2);
    assert!(
        unlocked.load(Ordering::SeqCst),
        "authenticated before locking"
    );
    assert_eq!(e.ra.events()[0], "ensure_authenticated");

    assert_eq!(
        events_after_editor(&e.ra),
        [
            "open_root None",
            "open_dir d @Some(1)",
            "open_file d/f1 @Some(1)",
            "text d/f1 base=true bytes=\"changed\\n\"",
            "close_file",
            "open_file d/f2 @Some(1)",
            "file_prop d/f2 p=Some(\"v\")",
            "close_file",
            "close_dir",
            "add_file n.txt",
            "text n.txt base=false bytes=\"new\\n\"",
            "close_file",
            "add_dir nd",
            "add_file nd/x",
            "text nd/x base=false bytes=\"x\\n\"",
            "close_file",
            "close_dir",
            "delete top.txt @Some(1)",
            "close_dir",
            "close_edit",
        ]
    );
    assert_eq!(e.server_file("d/f1").await, "changed\n");
    assert_eq!(e.server_file("nd/x").await, "x\n");
    assert!(e.ra.inner.open_txns().is_empty());

    // The working copy is valid for svn and fully folded.
    e.assert_clean_for_svn();
    let info = svn_out(
        &e.fx.oracle,
        &e.wc,
        &["info", "--show-item", "revision", "n.txt"],
    );
    assert_eq!(info.trim(), "2");
    let top = e.db.read_node(&rp("top.txt")).expect("read").expect("node");
    assert_eq!(top.presence, svn_wc::db::Presence::NotPresent);
    assert!(
        e.journal_files().is_empty(),
        "journal removed: {:?}",
        e.journal_files()
    );
    let diff = svn_out(&e.fx.oracle, &e.wc, &["diff"]);
    assert_eq!(diff, "", "svn diff is empty after commit");
}

#[tokio::test]
async fn nothing_to_commit_does_not_open_an_editor() {
    let Some(e) = env() else { return };
    assert!(e.commit_all("x").await.expect("commit").is_none());
    assert!(!e.ra.events().iter().any(|l| l.starts_with("commit_editor")));
    assert!(e.db.work_items().expect("wq").is_empty());
    assert_eq!(e.db.wc_lock_held(&rp("")).expect("held"), LockState::Free);
}

#[tokio::test]
async fn out_of_date_leaves_everything_untouched() {
    let Some(e) = env() else { return };
    e.ra.inner
        .seed(
            "other",
            "touch",
            &[FakeChange::PutFile(
                rp("d/f1"),
                Bytes::from_static(b"theirs\n"),
            )],
        )
        .expect("seed");
    write(&e.wc, "d/f1", "mine\n");
    let before = dump_rows(&e.wc);
    let err = e.commit_all("m").await.expect_err("out of date");
    assert_eq!(err.code, ErrorCode::OutOfDate, "{err}");
    assert_eq!(dump_rows(&e.wc), before, "WC rows unchanged");
    assert!(e.ra.inner.open_txns().is_empty(), "transaction deleted");
    assert!(e.journal_files().is_empty());
    assert_eq!(read(&e.wc, "d/f1"), "mine\n");
    assert_eq!(e.db.wc_lock_held(&rp("")).expect("held"), LockState::Free);
    assert!(e.ra.events().contains(&"abort_edit".to_owned()));
}

fn insert_lock_row(db: &WcDb, rel: &str, token: &str) {
    let lock = svn_wc::db::LockInfo {
        token: token.to_owned(),
        owner: Some("tester".to_owned()),
        comment: None,
        date: Some(1),
    };
    db.with_txn(|t| t.lock_add(&rp(rel), &lock))
        .expect("insert lock");
}

#[tokio::test]
async fn lock_tokens_are_sent_and_released() {
    let Some(e) = env() else { return };
    let lock =
        e.ra.inner
            .lock(&rp("d/f1"), None, false, None, &e.cancel)
            .await
            .expect("lock");
    insert_lock_row(&e.db, "d/f1", &lock.token);
    write(&e.wc, "d/f1", "locked edit\n");
    e.commit_all("m").await.expect("commit").expect("some");
    assert!(
        e.ra.events()
            .iter()
            .any(|l| l.contains("tokens=[(\"d/f1\"") && l.contains("keep_locks=false"))
    );
    assert!(
        e.ra.inner
            .get_locks(&rp(""), Depth::Infinity, &e.cancel)
            .await
            .expect("locks")
            .is_empty(),
        "server released the lock"
    );
    assert!(
        e.db.repos_lock(&rp("d/f1")).expect("lock").is_none(),
        "WC lock row gone"
    );
}

#[tokio::test]
async fn keep_locks_keeps_the_lock_everywhere() {
    let Some(e) = env() else { return };
    let lock =
        e.ra.inner
            .lock(&rp("d/f1"), None, false, None, &e.cancel)
            .await
            .expect("lock");
    insert_lock_row(&e.db, "d/f1", &lock.token);
    write(&e.wc, "d/f1", "locked edit\n");
    let mut opts = CommitOptions::new("m");
    opts.keep_locks = true;
    e.commit(std::slice::from_ref(&e.wc), &opts)
        .await
        .expect("commit");
    assert_eq!(
        e.ra.inner
            .get_locks(&rp(""), Depth::Infinity, &e.cancel)
            .await
            .expect("locks")
            .len(),
        1
    );
    assert!(e.db.repos_lock(&rp("d/f1")).expect("lock").is_some());
}

#[tokio::test]
async fn lock_held_by_someone_else_blocks_the_commit() {
    let Some(e) = env() else { return };
    let other = e.ra.inner.clone().with_user("other");
    other
        .lock(&rp("d/f1"), None, false, None, &e.cancel)
        .await
        .expect("lock");
    write(&e.wc, "d/f1", "mine\n");
    let before = dump_rows(&e.wc);
    let err = e.commit_all("m").await.expect_err("locked");
    assert_eq!(err.code, ErrorCode::LockedByOther, "{err}");
    assert_eq!(dump_rows(&e.wc), before);
    assert!(e.ra.inner.open_txns().is_empty());
}

#[tokio::test]
async fn cancel_before_merge_leaves_wc_and_server_untouched() {
    let Some(mut e) = env() else { return };
    write(&e.wc, "d/f1", "mine\n");
    write(&e.wc, "d/f2", "mine2\n");
    e.ra.cancel_on_text = Some(e.cancel.clone());
    let before = dump_rows(&e.wc);
    let err = e.commit_all("m").await.expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled, "{err}");
    assert_eq!(dump_rows(&e.wc), before);
    assert!(e.ra.inner.open_txns().is_empty());
    assert!(e.journal_files().is_empty());
    assert_eq!(
        e.ra.inner
            .youngest_rev(&CancelToken::new())
            .await
            .expect("rev")
            .0,
        1
    );
    assert!(e.db.work_items().expect("wq").is_empty());
}

#[tokio::test]
async fn unclassified_server_errors_are_mapped() {
    let Some(mut e) = env() else { return };
    write(&e.wc, "d/f1", "mine\n");
    e.ra.fail_on_text = Some(raw_error(160_024));
    let err = e.commit_all("m").await.expect_err("conflict");
    assert_eq!(err.code, ErrorCode::OutOfDate);
    assert!(e.ra.inner.open_txns().is_empty());
    for (code, expected) in [
        (160_035, ErrorCode::LockedByOther),
        (160_037, ErrorCode::LockTokenInvalid),
        (165_001, ErrorCode::HookFailed),
        (155_011, ErrorCode::OutOfDate),
    ] {
        assert_eq!(map_commit_error(raw_error(code)).code, expected);
    }
    assert_eq!(map_commit_error(raw_error(1)).code, ErrorCode::Internal);
}

#[tokio::test]
async fn conflicted_node_blocks_the_commit() {
    let Some(e) = env() else { return };
    let other = checkout(&e.fx.oracle, &e.fx.base, &e.fx.url, "other");
    write(&other, "top.txt", "theirs\n");
    svn(&e.fx.oracle, &other, &["commit", "-q", "-m", "o"]);
    // Conflict inside our own WC through svn.
    let wc2 = checkout(&e.fx.oracle, &e.fx.base, &e.fx.url, "wc2");
    svn(&e.fx.oracle, &wc2, &["update", "-q", "-r1"]);
    write(&wc2, "top.txt", "mine\n");
    svn(
        &e.fx.oracle,
        &wc2,
        &["update", "-q", "--accept", "postpone"],
    );
    let db2 = open(&wc2);
    let err = commit(
        &db2,
        &CommitContext {
            ra: &e.ra,
            state_dir: &e.state,
            cancel: &e.cancel,
        },
        std::slice::from_ref(&wc2),
        &CommitOptions::new("m"),
    )
    .await
    .expect_err("blocked");
    assert_eq!(err.code, ErrorCode::ConflictBlocksCommit, "{err}");
    assert!(!e.ra.events().iter().any(|l| l.starts_with("commit_editor")));
}

#[tokio::test]
async fn changelists_select_what_is_committed() {
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "one\n");
    write(&e.wc, "d/f2", "two\n");
    local::set_changelist(
        &e.db,
        &e.cancel,
        &[e.wc.join("d/f2")],
        Some("cl"),
        Depth::Empty,
    )
    .expect("cl");
    let mut opts = CommitOptions::new("m");
    opts.changelists = vec!["cl".into()];
    e.commit(std::slice::from_ref(&e.wc), &opts)
        .await
        .expect("commit")
        .expect("some");
    assert_eq!(e.server_file("d/f2").await, "two\n");
    assert_eq!(e.server_file("d/f1").await, "1\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc).trim(), "M       d/f1");
    // Membership is dropped after the commit by default.
    assert!(e.db.changelist(&rp("d/f2")).expect("cl").is_none());
}

#[tokio::test]
async fn keep_changelist_retains_membership() {
    let Some(e) = env() else { return };
    write(&e.wc, "d/f2", "two\n");
    local::set_changelist(
        &e.db,
        &e.cancel,
        &[e.wc.join("d/f2")],
        Some("cl"),
        Depth::Empty,
    )
    .expect("cl");
    let mut opts = CommitOptions::new("m");
    opts.keep_changelist = true;
    e.commit(std::slice::from_ref(&e.wc), &opts)
        .await
        .expect("commit");
    assert_eq!(
        e.db.changelist(&rp("d/f2")).expect("cl").as_deref(),
        Some("cl")
    );
}

#[tokio::test]
async fn depth_limits_the_harvest() {
    let Some(e) = env() else { return };
    write(&e.wc, "d/f1", "a\n");
    write(&e.wc, "d/sub/g", "b\n");
    let mut opts = CommitOptions::new("m");
    opts.depth = Depth::Files;
    e.commit(&[e.wc.join("d")], &opts).await.expect("commit");
    assert_eq!(e.server_file("d/f1").await, "a\n");
    assert_eq!(e.server_file("d/sub/g").await, "3\n");
    opts.depth = Depth::Empty;
    assert!(
        e.commit(&[e.wc.join("d")], &opts)
            .await
            .expect("commit")
            .is_none()
    );
}

#[tokio::test]
async fn added_child_without_its_added_parent_is_refused() {
    let Some(e) = env() else { return };
    write(&e.wc, "nd/x", "x\n");
    e.add(&["nd"]);
    let err = e
        .commit(&[e.wc.join("nd/x")], &CommitOptions::new("m"))
        .await
        .expect_err("parent missing");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{err}");
}

#[tokio::test]
async fn copies_moves_and_replacements_are_driven_with_history() {
    let Some(mut e) = env() else { return };
    e.ra.record_only = true;
    local::copy(&e.db, &e.cancel, &e.wc.join("d"), &e.wc.join("dcopy")).expect("cp");
    write(&e.wc, "dcopy/f2", "edited copy\n");
    local::move_path(
        &e.db,
        &e.cancel,
        &e.wc.join("top.txt"),
        &e.wc.join("moved.txt"),
    )
    .expect("mv");
    local::delete(
        &e.db,
        &e.cancel,
        &[e.wc.join("e")],
        &DeleteOptions::default(),
    )
    .expect("rm");
    std::fs::create_dir(e.wc.join("e")).expect("mkdir");
    e.add(&["e"]);

    e.commit_all("history")
        .await
        .expect("commit")
        .expect("some");
    let ev = events_after_editor(&e.ra);
    let url = "https://svn.test.local/repo";
    for want in [
        format!("add_dir dcopy copy={url}/d@1"),
        format!("add_file moved.txt copy={url}/top.txt@1"),
        "open_file dcopy/f2 @None".to_owned(),
        "text dcopy/f2 base=true bytes=\"edited copy\\n\"".to_owned(),
        "delete top.txt @Some(1)".to_owned(),
        "delete e @Some(1)".to_owned(),
        "add_dir e".to_owned(),
    ] {
        assert!(ev.contains(&want), "missing {want:?} in {ev:#?}");
    }
    let del = ev
        .iter()
        .position(|l| l == "delete e @Some(1)")
        .expect("del");
    assert_eq!(ev[del + 1], "add_dir e", "replace is delete then add");
    // svn accepts the folded copy history: nothing local left, mixed revisions are fine.
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
    let info = svn_out(&e.fx.oracle, &e.wc, &["info", "dcopy"]);
    assert!(info.contains("Revision: 2"), "{info}");
    assert!(
        !info.contains("Copied From"),
        "history is committed: {info}"
    );
}

#[tokio::test]
async fn eol_and_keywords_are_normalized_and_reexpanded_like_svn() {
    let Some(mut e) = env() else { return };
    e.ra.record_only = true;
    let content = "line1\r\n$Rev$\r\n";
    write(&e.wc, "k.txt", content);
    e.add(&["k.txt"]);
    e.propset("k.txt", "svn:eol-style", "native");
    e.propset("k.txt", "svn:keywords", "Rev");
    e.commit_all("m").await.expect("commit").expect("some");
    let ev = events_after_editor(&e.ra);
    assert!(
        ev.contains(&"text k.txt base=false bytes=\"line1\\n$Rev$\\n\"".to_owned()),
        "{ev:#?}"
    );
    assert!(ev.contains(&"file_prop k.txt svn:eol-style=Some(\"native\")".to_owned()));
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
    // svn performs the same operation on the real repository: same text on disk afterwards.
    let wc2 = checkout(&e.fx.oracle, &e.fx.base, &e.fx.url, "svn-side");
    write(&wc2, "k.txt", content);
    svn(&e.fx.oracle, &wc2, &["add", "-q", "k.txt"]);
    svn(
        &e.fx.oracle,
        &wc2,
        &["propset", "-q", "svn:eol-style", "native", "k.txt"],
    );
    svn(
        &e.fx.oracle,
        &wc2,
        &["propset", "-q", "svn:keywords", "Rev", "k.txt"],
    );
    svn(&e.fx.oracle, &wc2, &["commit", "-q", "-m", "m"]);
    assert_eq!(read(&e.wc, "k.txt"), read(&wc2, "k.txt"));
    assert!(read(&e.wc, "k.txt").contains("$Rev: 2 $"));
}

#[cfg(unix)]
#[tokio::test]
async fn needs_lock_files_become_read_only_after_commit() {
    let Some(mut e) = env() else { return };
    e.ra.record_only = true;
    write(&e.wc, "ro.txt", "x\n");
    e.add(&["ro.txt"]);
    e.propset("ro.txt", "svn:needs-lock", "*");
    e.commit_all("m").await.expect("commit");
    let md = std::fs::metadata(e.wc.join("ro.txt")).expect("md");
    assert!(md.permissions().readonly());
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}

#[tokio::test]
async fn crash_artifacts_are_recovered_from_journals() {
    use svn_client::commit::journal::{JournalObserver, JournalStore};
    use svn_client::commit::recover_commits;
    let Some(e) = env() else { return };
    // A journal that only names a leaked transaction: recovery deletes it on the server.
    let store = JournalStore::new(&e.state);
    let id = store.new_id();
    let data = store.begin(
        id.clone(),
        e.wc.clone(),
        false,
        false,
        Vec::new(),
        Vec::new(),
    );
    store.write(&data).expect("journal");
    let obs = Arc::new(JournalObserver::new(store.clone(), data));
    let editor =
        e.ra.inner
            .commit_editor("m", &[], &[], false, obs, &e.cancel)
            .await
            .expect("editor");
    assert_eq!(e.ra.inner.open_txns().len(), 1);
    std::mem::forget(editor); // the process "died" with the transaction open
    let wcpath = e.wc.clone();
    let open_wc = move |p: &std::path::Path| -> Result<WcDb> {
        assert_eq!(p, wcpath);
        Ok(open(p))
    };
    let report = recover_commits(&e.state, &e.ra, &e.cancel, &open_wc)
        .await
        .expect("recover");
    assert_eq!(report.aborted, vec![id]);
    assert!(report.failed.is_empty());
    assert!(e.ra.inner.open_txns().is_empty());
    assert!(e.journal_files().is_empty());
}

#[tokio::test]
async fn map_error_keeps_specific_codes() {
    let e = raw_error(160_042);
    assert_eq!(map_commit_error(e).code, ErrorCode::OutOfDate);
    let already = svn_types::SvnError::new(ErrorCode::Network, "x").with_svn_code(160_024);
    assert_eq!(map_commit_error(already).code, ErrorCode::Network);
}

/// Hides server-specific date and author, which differ between the real and the fake server.
fn mask_commit_metadata(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|r| match (r.find(" cd="), r.find(" fe=")) {
            (Some(a), Some(b)) if a < b => format!("{} cd=_ ca=_{}", &r[..a], &r[b..]),
            _ => r,
        })
        .collect()
}

/// The same changes are committed by svn on one checkout and by us on another; the resulting
/// wc.db rows (BASE, pristines, ACTUAL) must be identical.
#[tokio::test]
async fn post_commit_rows_match_svn() {
    let Some(mut e) = env() else { return };
    e.ra.record_only = true;
    let svn_wc = checkout(&e.fx.oracle, &e.fx.base, &e.fx.url, "svn-side");

    // svn side
    write(&svn_wc, "d/f1", "changed\n");
    write(&svn_wc, "n.txt", "new\n");
    write(&svn_wc, "nd/x", "x\n");
    write(&svn_wc, "nd/y", "y\n");
    let o = &e.fx.oracle;
    svn(o, &svn_wc, &["add", "-q", "n.txt", "nd"]);
    svn(o, &svn_wc, &["propset", "-q", "p", "v", "d/f2"]);
    svn(o, &svn_wc, &["propset", "-q", "dp", "1", "d"]);
    svn(o, &svn_wc, &["cp", "-q", "d/sub", "subcopy"]);
    svn(o, &svn_wc, &["mv", "-q", "top.txt", "moved.txt"]);
    svn(o, &svn_wc, &["rm", "-q", "e"]);
    write(&svn_wc, "e/again", "a\n");
    svn(o, &svn_wc, &["add", "-q", "e"]);
    svn(o, &svn_wc, &["changelist", "-q", "cl", "d/f2"]);
    svn(o, &svn_wc, &["commit", "-q", "-m", "m"]);

    // our side
    write(&e.wc, "d/f1", "changed\n");
    write(&e.wc, "n.txt", "new\n");
    write(&e.wc, "nd/x", "x\n");
    write(&e.wc, "nd/y", "y\n");
    e.add(&["n.txt", "nd"]);
    e.propset("d/f2", "p", "v");
    e.propset("d", "dp", "1");
    local::copy(&e.db, &e.cancel, &e.wc.join("d/sub"), &e.wc.join("subcopy")).expect("cp");
    local::move_path(
        &e.db,
        &e.cancel,
        &e.wc.join("top.txt"),
        &e.wc.join("moved.txt"),
    )
    .expect("mv");
    local::delete(
        &e.db,
        &e.cancel,
        &[e.wc.join("e")],
        &DeleteOptions::default(),
    )
    .expect("rm");
    write(&e.wc, "e/again", "a\n");
    e.add(&["e"]);
    local::set_changelist(
        &e.db,
        &e.cancel,
        &[e.wc.join("d/f2")],
        Some("cl"),
        Depth::Empty,
    )
    .expect("cl");
    e.commit_all("m").await.expect("commit").expect("some");

    assert_rows_equal(
        "post-commit",
        &mask_commit_metadata(dump_rows(&svn_wc)),
        &mask_commit_metadata(dump_rows(&e.wc)),
    );
    assert_eq!(svn_status(o, &svn_wc), svn_status(o, &e.wc));
}

/// Repository paths of the cached locks in wc.db.
fn lock_rows(wc: &std::path::Path) -> Vec<String> {
    let db = WcDb::open(wc).expect("open");
    db.with_read(|c| {
        let mut st = c
            .prepare("SELECT repos_relpath FROM lock ORDER BY repos_relpath")
            .expect("prepare");
        let rows = st.query_map([], |r| r.get::<_, String>(0)).expect("query");
        Ok(rows.map(|r| r.expect("row")).collect())
    })
    .expect("read")
}

/// Paths locked in the oracle's `file://` repository.
fn svn_locks(e: &Env) -> Vec<String> {
    let out = std::process::Command::new(
        std::env::var("OPENSVN_SVNADMIN_BIN").unwrap_or_else(|_| "svnadmin".into()),
    )
    .args(["lslocks", "repo"])
    .current_dir(&e.fx.base)
    .output()
    .expect("svnadmin lslocks");
    let mut paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("Path: /"))
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths
}

async fn server_locks(e: &Env) -> Vec<String> {
    let mut v: Vec<String> =
        e.ra.inner
            .get_locks(&rp(""), Depth::Infinity, &e.cancel)
            .await
            .expect("locks")
            .into_iter()
            .map(|l| l.path.as_str().trim_start_matches('/').to_owned())
            .collect();
    v.sort();
    v
}

/// Locks `rels` on the fake server and caches the tokens in our working copy.
async fn lock_ours(e: &Env, rels: &[&str]) -> Vec<String> {
    let mut tokens = Vec::new();
    for rel in rels {
        let lock =
            e.ra.inner
                .lock(&rp(rel), None, false, None, &e.cancel)
                .await
                .expect("lock");
        insert_lock_row(&e.db, rel, &lock.token);
        tokens.push(lock.token);
    }
    tokens
}

/// svn 1.15 sends the tokens of every locked node below a deleted directory (the server checks
/// them recursively, like FSFS `verify_locks`) and releases them all.
#[tokio::test]
async fn deleting_a_locked_tree_sends_and_releases_every_token() {
    let Some(e) = env() else { return };
    let o = &e.fx.oracle;
    let svn_wc = checkout(o, &e.fx.base, &e.fx.url, "svn-side");
    svn(o, &svn_wc, &["lock", "-q", "d/f1", "d/sub/g"]);
    svn(o, &svn_wc, &["rm", "-q", "d"]);
    svn(o, &svn_wc, &["commit", "-q", "-m", "rm"]);
    assert!(svn_locks(&e).is_empty(), "svn released the locks");
    assert!(lock_rows(&svn_wc).is_empty());

    let tokens = lock_ours(&e, &["d/f1", "d/sub/g"]).await;
    local::delete(
        &e.db,
        &e.cancel,
        &[e.wc.join("d")],
        &DeleteOptions::default(),
    )
    .expect("rm");
    e.commit_all("rm").await.expect("commit").expect("some");
    let ev = e.ra.events();
    let editor = ev
        .iter()
        .find(|l| l.starts_with("commit_editor"))
        .expect("editor");
    for t in &tokens {
        assert!(editor.contains(t.as_str()), "token {t} sent: {editor}");
    }
    assert!(server_locks(&e).await.is_empty(), "server released them");
    assert_eq!(
        lock_rows(&e.wc),
        lock_rows(&svn_wc),
        "no orphaned lock rows"
    );
    assert_eq!(svn_status(o, &e.wc), svn_status(o, &svn_wc));
}

/// svn 1.15 releases the locks of unmodified locked files met by the commit walk too, without
/// touching their BASE; outside a changelist filter they stay.
#[tokio::test]
async fn unmodified_locked_files_below_the_targets_are_released() {
    let Some(e) = env() else { return };
    let o = &e.fx.oracle;
    let svn_wc = checkout(o, &e.fx.base, &e.fx.url, "svn-side");
    svn(o, &svn_wc, &["lock", "-q", "top.txt", "d/f1"]);
    write(&svn_wc, "d/f2", "changed\n");
    svn(o, &svn_wc, &["commit", "-q", "-m", "m"]);
    assert!(svn_locks(&e).is_empty());

    let tokens = lock_ours(&e, &["top.txt", "d/f1"]).await;
    write(&e.wc, "d/f2", "changed\n");
    e.commit_all("m").await.expect("commit").expect("some");
    let ev = e.ra.events();
    let editor = ev
        .iter()
        .find(|l| l.starts_with("commit_editor"))
        .expect("editor");
    for t in &tokens {
        assert!(editor.contains(t.as_str()), "token {t} sent: {editor}");
    }
    assert!(server_locks(&e).await.is_empty(), "server released them");
    assert_eq!(lock_rows(&e.wc), lock_rows(&svn_wc));
    assert_rows_equal(
        "locks-released",
        &mask_commit_metadata(dump_rows(&svn_wc)),
        &mask_commit_metadata(dump_rows(&e.wc)),
    );
    assert_eq!(svn_status(o, &e.wc), svn_status(o, &svn_wc));
}

#[tokio::test]
async fn locks_outside_a_changelist_filter_are_kept() {
    let Some(e) = env() else { return };
    let o = &e.fx.oracle;
    let svn_wc = checkout(o, &e.fx.base, &e.fx.url, "svn-side");
    svn(o, &svn_wc, &["lock", "-q", "top.txt"]);
    write(&svn_wc, "d/f2", "changed\n");
    svn(o, &svn_wc, &["changelist", "-q", "cl", "d/f2"]);
    svn(
        o,
        &svn_wc,
        &["commit", "-q", "-m", "m", "--changelist", "cl", "."],
    );
    assert_eq!(svn_locks(&e), vec!["top.txt".to_owned()]);

    lock_ours(&e, &["top.txt"]).await;
    write(&e.wc, "d/f2", "changed\n");
    local::set_changelist(
        &e.db,
        &e.cancel,
        &[e.wc.join("d/f2")],
        Some("cl"),
        Depth::Empty,
    )
    .expect("cl");
    let mut opts = CommitOptions::new("m");
    opts.changelists = vec!["cl".into()];
    e.commit(std::slice::from_ref(&e.wc), &opts)
        .await
        .expect("commit")
        .expect("some");
    assert_eq!(server_locks(&e).await, vec!["top.txt".to_owned()]);
    assert_eq!(lock_rows(&e.wc), lock_rows(&svn_wc));
}

fn svn_fails(e: &Env, wc: &std::path::Path, args: &[&str]) -> String {
    let out =
        e.fx.oracle
            .svn()
            .args(args)
            .current_dir(wc)
            .output()
            .expect("svn");
    assert!(!out.status.success(), "svn {args:?} succeeded");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// svn 1.15: "Cannot commit '…' because it was moved from/to '…' which is not part of the
/// commit; both sides of the move must be committed together" (E200009), before any server
/// contact.
#[tokio::test]
async fn one_side_of_a_move_cannot_be_committed_alone() {
    let Some(e) = env() else { return };
    let o = &e.fx.oracle;
    let svn_wc = checkout(o, &e.fx.base, &e.fx.url, "svn-side");
    svn(o, &svn_wc, &["mv", "-q", "top.txt", "t2.txt"]);
    svn(o, &svn_wc, &["mv", "-q", "d/sub", "d/sub2"]);
    for side in ["t2.txt", "top.txt", "d/sub2", "d/sub"] {
        let err = svn_fails(&e, &svn_wc, &["commit", "-m", "m", side]);
        assert!(err.contains("E200009"), "{side}: {err}");
    }

    local::move_path(
        &e.db,
        &e.cancel,
        &e.wc.join("top.txt"),
        &e.wc.join("t2.txt"),
    )
    .expect("mv");
    local::move_path(&e.db, &e.cancel, &e.wc.join("d/sub"), &e.wc.join("d/sub2")).expect("mv");
    let before = dump_rows(&e.wc);
    for side in ["t2.txt", "top.txt", "d/sub2", "d/sub"] {
        let err = e
            .commit(&[e.wc.join(side)], &CommitOptions::new("m"))
            .await
            .expect_err(side);
        assert_eq!(err.svn_code, Some(200_009), "{side}: {err}");
        assert!(err.message.contains("both sides of the move"), "{err}");
    }
    assert!(
        !e.ra.events().iter().any(|l| l.starts_with("commit_editor")),
        "refused before contacting the server"
    );
    assert_eq!(dump_rows(&e.wc), before);
    // Both sides together are fine.
    e.commit(
        &[e.wc.join("t2.txt"), e.wc.join("top.txt")],
        &CommitOptions::new("m"),
    )
    .await
    .expect("commit")
    .expect("some");
}

/// A change below a copy whose root is not committed: svn refuses before the server ("is not
/// known to exist in the repository and is not part of the commit").
#[tokio::test]
async fn change_inside_an_uncommitted_copy_is_refused() {
    let Some(e) = env() else { return };
    let o = &e.fx.oracle;
    let svn_wc = checkout(o, &e.fx.base, &e.fx.url, "svn-side");
    svn(o, &svn_wc, &["cp", "-q", "d", "dc"]);
    write(&svn_wc, "dc/f2", "edited\n");
    let err = svn_fails(&e, &svn_wc, &["commit", "-m", "m", "dc/f2"]);
    assert!(
        err.contains("is not known to exist in the repository"),
        "{err}"
    );

    local::copy(&e.db, &e.cancel, &e.wc.join("d"), &e.wc.join("dc")).expect("cp");
    write(&e.wc, "dc/f2", "edited\n");
    let err = e
        .commit(&[e.wc.join("dc/f2")], &CommitOptions::new("m"))
        .await
        .expect_err("refused");
    assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{err}");
    assert!(err.message.contains("is not known to exist"), "{err}");
    assert!(!e.ra.events().iter().any(|l| l.starts_with("commit_editor")));
}

/// A property change on the session root is made against its BASE revision, so an outdated
/// root is rejected like svn's commit does.
#[tokio::test]
async fn root_property_change_on_an_outdated_root_is_out_of_date() {
    let Some(e) = env() else { return };
    let o = &e.fx.oracle;
    let svn_wc = checkout(o, &e.fx.base, &e.fx.url, "svn-side");
    let other = checkout(o, &e.fx.base, &e.fx.url, "other");
    write(&other, "d/f1", "theirs\n");
    svn(o, &other, &["commit", "-q", "-m", "other"]);
    svn(o, &svn_wc, &["propset", "-q", "rp", "1", "."]);
    let err = svn_fails(&e, &svn_wc, &["commit", "-m", "m"]);
    assert!(err.contains("out of date"), "{err}");

    e.ra.inner
        .seed(
            "other",
            "touch",
            &[FakeChange::PutFile(
                rp("d/f1"),
                Bytes::from_static(b"theirs\n"),
            )],
        )
        .expect("seed");
    e.propset("", "rp", "1");
    let err = e.commit_all("m").await.expect_err("out of date");
    assert_eq!(err.code, ErrorCode::OutOfDate, "{err}");
    assert!(e.journal_files().is_empty());
}

/// Cancelling once the server merged the commit does not turn it into a failure: the commit
/// is reported, the working copy is finished and no journal stays behind.
#[tokio::test]
async fn cancel_after_merge_still_finishes_the_commit() {
    let Some(mut e) = env() else { return };
    e.ra.cancel_after_merge = Some(e.cancel.clone());
    write(&e.wc, "d/f1", "rev $Rev$\n");
    e.propset("d/f1", "svn:keywords", "Rev");
    let res = e
        .commit_all("m")
        .await
        .expect("a merged commit is never reported as failed")
        .expect("some");
    assert_eq!(res.revision.0, 2);
    assert_eq!(res.wc_warning, None);
    assert!(e.cancel.is_cancelled());
    assert!(e.journal_files().is_empty(), "no stale journal");
    assert!(e.db.work_items().expect("wq").is_empty());
    assert_eq!(read(&e.wc, "d/f1"), "rev $Rev: 2 $\n");
    assert_eq!(svn_status(&e.fx.oracle, &e.wc), "");
}
