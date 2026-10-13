//! Commit editor, locks, reads and authentication of the fake session.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use secrecy::SecretString;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CommitInfo, CommitObserver,
    CredentialRequest, Credentials, FileRev, FileRevHandler, LogRequest, PromptReason, RaSession,
    ReplayableSource,
};
use svn_types::{
    CancelToken, Checksum, Depth, ErrorCode, NodeKind, PropChange, RelPath, Result, Revnum,
    TxDeltaWindow, WindowSink,
};

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("rel path")
}

fn put(path: &str, content: &str) -> FakeChange {
    FakeChange::PutFile(p(path), Bytes::from(content.to_owned()))
}

fn session() -> FakeRaSession {
    let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1").expect("session");
    s.seed(
        "alice",
        "first",
        &[
            put("trunk/a.txt", "one"),
            put("trunk/b.txt", "bee"),
            FakeChange::PutDir(p("branches")),
        ],
    )
    .expect("r1");
    s.seed("bob", "second", &[put("trunk/a.txt", "two")])
        .expect("r2");
    s
}

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl CommitObserver for Recorder {
    fn on_txn_created(&self, txn: &str) -> Result<()> {
        self.0.lock().unwrap().push(format!("txn:{txn}"));
        Ok(())
    }
    fn on_merged(&self, info: &CommitInfo) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("merged:{}", info.revision));
        Ok(())
    }
}

fn source(dir: &tempfile::TempDir, name: &str, content: &str) -> ReplayableSource {
    let path = dir.path().join(name);
    std::fs::write(&path, content).expect("write source");
    ReplayableSource {
        path,
        translated: true,
        size: Some(content.len() as u64),
    }
}

async fn cat(s: &FakeRaSession, path: &str, rev: Option<Revnum>) -> Vec<u8> {
    let mut out = Vec::new();
    s.get_file(&p(path), rev, &mut out, &CancelToken::new())
        .await
        .expect("get_file");
    out
}

#[tokio::test]
async fn commit_creates_a_new_revision() {
    let s = session();
    let tmp = tempfile::tempdir().expect("tmp");
    let obs = Arc::new(Recorder::default());
    let mut ed = s
        .commit_editor(
            "update files",
            &[],
            &[],
            false,
            obs.clone(),
            &CancelToken::new(),
        )
        .await
        .expect("editor");

    let root = ed.open_root(Some(Revnum(2))).await.expect("root");
    let trunk = ed
        .open_directory(&p("trunk"), root, Some(Revnum(2)))
        .await
        .expect("trunk");
    let a = ed
        .open_file(&p("trunk/a.txt"), trunk, Some(Revnum(2)))
        .await
        .expect("a");
    ed.apply_text(
        a,
        Some(source(&tmp, "base", "two")),
        source(&tmp, "a", "three"),
        Some(Checksum::Md5([0; 16])),
        Checksum::Md5([1; 16]),
    )
    .await
    .expect("text");
    ed.change_file_prop(
        a,
        PropChange {
            name: "svn:eol-style".into(),
            value: Some("LF".into()),
        },
    )
    .await
    .expect("prop");
    ed.close_file(a).await.expect("close a");
    let c = ed
        .add_file(&p("trunk/c.txt"), trunk, None)
        .await
        .expect("c");
    ed.apply_text(
        c,
        None,
        source(&tmp, "c", "new file"),
        None,
        Checksum::Md5([2; 16]),
    )
    .await
    .expect("text c");
    ed.close_file(c).await.expect("close c");
    ed.delete_entry(&p("trunk/b.txt"), Some(Revnum(2)), trunk)
        .await
        .expect("delete b");
    ed.close_directory(trunk).await.expect("close trunk");
    ed.close_directory(root).await.expect("close root");
    assert_eq!(
        s.open_txns().len(),
        1,
        "transaction exists before the merge"
    );

    let info = ed.close_edit().await.expect("merge");
    assert_eq!(info.revision, Revnum(3));
    assert_eq!(info.author.as_deref(), Some("tester"));
    assert!(s.open_txns().is_empty());
    assert_eq!(
        s.youngest_rev(&CancelToken::new()).await.expect("youngest"),
        Revnum(3)
    );
    assert_eq!(
        *obs.0.lock().unwrap(),
        vec!["txn:1-fake".to_owned(), "merged:r3".to_owned()]
    );

    assert_eq!(cat(&s, "trunk/a.txt", None).await, b"three");
    assert_eq!(
        cat(&s, "trunk/a.txt", Some(Revnum(2))).await,
        b"two",
        "history is preserved"
    );
    assert_eq!(cat(&s, "trunk/c.txt", None).await, b"new file");
    let c = CancelToken::new();
    assert_eq!(
        s.check_path(&p("trunk/b.txt"), None, &c)
            .await
            .expect("kind"),
        NodeKind::None
    );
    assert_eq!(
        s.check_path(&p("trunk/b.txt"), Some(Revnum(2)), &c)
            .await
            .expect("kind"),
        NodeKind::File
    );

    let mut seen = Vec::new();
    let req = LogRequest {
        paths: vec![],
        start: None,
        end: None,
        limit: Some(1),
        discover_changed_paths: true,
        strict_node_history: false,
    };
    s.log(
        &req,
        &mut |e| {
            seen.push(e);
            Ok(())
        },
        &c,
    )
    .await
    .expect("log");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].rev, Revnum(3));
    assert_eq!(seen[0].message.as_deref(), Some("update files"));
    assert!(
        seen[0]
            .changed_paths
            .iter()
            .any(|c| c.path.as_str() == "/trunk/b.txt")
    );
}

#[tokio::test]
async fn abort_removes_the_transaction_and_keeps_the_head() {
    let s = session();
    let ed = s
        .commit_editor(
            "m",
            &[],
            &[],
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    assert_eq!(s.open_txns().len(), 1);
    ed.abort_edit().await.expect("abort");
    assert!(s.open_txns().is_empty());
    assert_eq!(
        s.youngest_rev(&CancelToken::new()).await.expect("youngest"),
        Revnum(2)
    );

    // A leaked transaction can be cleaned up by name, repeatedly.
    let _leaked = s
        .commit_editor(
            "m",
            &[],
            &[],
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    let name = s.open_txns().remove(0);
    s.abort_txn(&name).await.expect("abort_txn");
    s.abort_txn(&name).await.expect("idempotent");
    assert!(s.open_txns().is_empty());
}

#[tokio::test]
async fn out_of_date_edits_are_rejected() {
    let s = session();
    let mut ed = s
        .commit_editor(
            "m",
            &[],
            &[],
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    let root = ed.open_root(Some(Revnum(2))).await.expect("root");
    let trunk = ed.open_directory(&p("trunk"), root, Some(Revnum(1))).await;
    assert_eq!(
        trunk.expect_err("trunk changed in r2").code,
        ErrorCode::OutOfDate
    );
    let trunk = ed
        .open_directory(&p("trunk"), root, Some(Revnum(2)))
        .await
        .expect("trunk at r2");
    let err = ed
        .open_file(&p("trunk/a.txt"), trunk, Some(Revnum(1)))
        .await
        .expect_err("a changed in r2");
    assert_eq!(err.code, ErrorCode::OutOfDate);
    let err = ed
        .add_file(&p("trunk/a.txt"), trunk, None)
        .await
        .expect_err("exists");
    assert_eq!(err.code, ErrorCode::Internal);
}

#[tokio::test]
async fn racing_commit_is_refused_at_merge() {
    let s = session();
    let ed = s
        .commit_editor(
            "m",
            &[],
            &[],
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    s.seed("carol", "meanwhile", &[put("other.txt", "x")])
        .expect("seed");
    let err = ed.close_edit().await.expect_err("out of date");
    assert_eq!(err.code, ErrorCode::OutOfDate);
}

#[tokio::test]
async fn locks_gate_commits_and_are_released_on_merge() {
    let s = session();
    let c = CancelToken::new();
    let lock = s
        .lock(
            &p("trunk/a.txt"),
            Some("editing"),
            false,
            Some(Revnum(2)),
            &c,
        )
        .await
        .expect("lock");
    assert_eq!(lock.path.as_str(), "/trunk/a.txt");
    assert_eq!(lock.owner, "tester");
    assert_eq!(
        s.lock(&p("trunk/a.txt"), None, false, None, &c)
            .await
            .expect_err("held")
            .code,
        ErrorCode::LockedByOther
    );
    assert_eq!(
        s.get_locks(&p(""), Depth::Infinity, &c)
            .await
            .expect("locks"),
        vec![lock.clone()]
    );

    // Without the token the file cannot be opened for edit.
    let mut ed = s
        .commit_editor(
            "m",
            &[],
            &[],
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    let err = ed
        .open_file(&p("trunk/a.txt"), root, None)
        .await
        .expect_err("locked");
    assert_eq!(err.code, ErrorCode::LockedByOther);
    ed.abort_edit().await.expect("abort");

    // Wrong token.
    let bad = vec![("trunk/a.txt".to_owned(), "opaquelocktoken:bad".to_owned())];
    let mut ed = s
        .commit_editor(
            "m",
            &[],
            &bad,
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    assert_eq!(
        ed.open_file(&p("trunk/a.txt"), root, None)
            .await
            .expect_err("bad")
            .code,
        ErrorCode::LockTokenInvalid
    );
    ed.abort_edit().await.expect("abort");

    // Right token; lock released by the commit because keep_locks is false.
    let tokens = vec![("trunk/a.txt".to_owned(), lock.token.clone())];
    let tmp = tempfile::tempdir().expect("tmp");
    let mut ed = s
        .commit_editor(
            "m",
            &[],
            &tokens,
            false,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    let f = ed
        .open_file(&p("trunk/a.txt"), root, None)
        .await
        .expect("open");
    ed.apply_text(
        f,
        None,
        source(&tmp, "t", "locked edit"),
        None,
        Checksum::Md5([0; 16]),
    )
    .await
    .expect("text");
    ed.close_edit().await.expect("merge");
    assert!(
        s.get_locks(&p(""), Depth::Infinity, &c)
            .await
            .expect("locks")
            .is_empty()
    );
}

#[tokio::test]
async fn keep_locks_and_steal_and_unlock() {
    let s = session();
    let c = CancelToken::new();
    let first = s
        .lock(&p("trunk/b.txt"), None, false, None, &c)
        .await
        .expect("lock");
    let stolen = s
        .lock(&p("trunk/b.txt"), None, true, None, &c)
        .await
        .expect("steal");
    assert_ne!(first.token, stolen.token);
    assert_eq!(
        s.unlock(&p("trunk/b.txt"), Some(&first.token), false, &c)
            .await
            .expect_err("old token")
            .code,
        ErrorCode::LockTokenInvalid
    );
    s.unlock(&p("trunk/b.txt"), None, true, &c)
        .await
        .expect("break");
    assert!(
        s.get_locks(&p(""), Depth::Infinity, &c)
            .await
            .expect("none")
            .is_empty()
    );
    assert_eq!(
        s.lock(&p("trunk"), None, false, None, &c)
            .await
            .expect_err("dir")
            .code,
        ErrorCode::PathNotFound
    );

    let l = s
        .lock(&p("trunk/a.txt"), None, false, None, &c)
        .await
        .expect("lock");
    let tokens = vec![("trunk/a.txt".to_owned(), l.token.clone())];
    let mut ed = s
        .commit_editor(
            "m",
            &[],
            &tokens,
            true,
            Arc::new(Recorder::default()),
            &CancelToken::new(),
        )
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    let f = ed
        .open_file(&p("trunk/a.txt"), root, None)
        .await
        .expect("open");
    ed.change_file_prop(
        f,
        PropChange {
            name: "x".into(),
            value: Some("1".into()),
        },
    )
    .await
    .expect("prop");
    ed.close_edit().await.expect("merge");
    assert_eq!(
        s.get_locks(&p(""), Depth::Infinity, &c)
            .await
            .expect("kept")
            .len(),
        1
    );
}

#[tokio::test]
async fn read_operations() {
    let s = session();
    let c = CancelToken::new();
    assert_eq!(s.uuid(), "uuid-1");
    assert_eq!(s.repos_root(), s.session_url());
    assert_eq!(
        s.check_path(&p("trunk"), None, &c).await.expect("kind"),
        NodeKind::Dir
    );
    assert_eq!(
        s.check_path(&p("nope"), None, &c).await.expect("kind"),
        NodeKind::None
    );
    let st = s
        .stat(&p("trunk/a.txt"), None, &c)
        .await
        .expect("stat")
        .expect("exists");
    assert_eq!(
        (st.name.as_str(), st.kind, st.size, st.created_rev),
        ("a.txt", NodeKind::File, Some(3), Some(Revnum(2)))
    );
    assert!(s.stat(&p("zzz"), None, &c).await.expect("stat").is_none());

    let names = |v: Vec<svn_types::DirEntry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
    assert_eq!(
        names(
            s.list(&p(""), None, Depth::Immediates, &c)
                .await
                .expect("list")
        ),
        ["branches", "trunk"]
    );
    assert_eq!(
        names(s.list(&p(""), None, Depth::Files, &c).await.expect("list")),
        Vec::<String>::new()
    );
    assert_eq!(
        names(
            s.list(&p("trunk"), None, Depth::Infinity, &c)
                .await
                .expect("list")
        ),
        ["a.txt", "b.txt"]
    );
    assert_eq!(
        s.list(&p("trunk/a.txt"), None, Depth::Immediates, &c)
            .await
            .expect_err("file")
            .code,
        ErrorCode::PathNotFound
    );
    assert_eq!(
        s.list(&p("missing"), None, Depth::Immediates, &c)
            .await
            .expect_err("missing")
            .code,
        ErrorCode::PathNotFound
    );
    assert_eq!(
        s.get_file(&p("trunk/a.txt"), Some(Revnum(99)), &mut Vec::new(), &c)
            .await
            .expect_err("rev")
            .code,
        ErrorCode::PathNotFound
    );

    // Log: newest first, path filter, limit.
    let mut revs = Vec::new();
    let mut req = LogRequest {
        paths: vec![p("trunk/a.txt")],
        start: None,
        end: None,
        limit: None,
        discover_changed_paths: false,
        strict_node_history: false,
    };
    s.log(
        &req,
        &mut |e| {
            revs.push(e.rev.0);
            Ok(())
        },
        &c,
    )
    .await
    .expect("log");
    assert_eq!(revs, [2, 1]);
    req.paths = vec![p("branches")];
    revs.clear();
    s.log(
        &req,
        &mut |e| {
            revs.push(e.rev.0);
            Ok(())
        },
        &c,
    )
    .await
    .expect("log");
    assert_eq!(revs, [1]);
}

type Seen = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

struct Collect(Seen, u64);

impl WindowSink for Collect {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        if let Some(w) = w {
            self.0.lock().unwrap().push((self.1, w.new_data.to_vec()));
        }
        Ok(())
    }
}

struct Handler(Seen);

impl FileRevHandler for Handler {
    fn revision(&mut self, rev: FileRev) -> Result<Option<Box<dyn WindowSink>>> {
        Ok(Some(Box::new(Collect(self.0.clone(), rev.rev.0))))
    }
}

#[tokio::test]
async fn file_revs_streams_each_revision_that_changed_the_file() {
    let s = session();
    let got = Arc::new(Mutex::new(Vec::new()));
    s.file_revs(
        &p("trunk/a.txt"),
        None,
        None,
        &mut Handler(got.clone()),
        &CancelToken::new(),
    )
    .await
    .expect("file_revs");
    assert_eq!(
        *got.lock().unwrap(),
        vec![(1, b"one".to_vec()), (2, b"two".to_vec())]
    );
}

struct Prompter {
    answers: Mutex<Vec<Option<&'static str>>>,
    asked: Mutex<Vec<u32>>,
}

#[async_trait]
impl AuthPrompter for Prompter {
    async fn credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        assert_eq!(req.origin.host, "svn.test.local");
        assert_eq!(req.origin.port, 443);
        assert_eq!(req.offered, [AuthMethod::Basic]);
        let expected = if req.attempt == 0 {
            PromptReason::Initial
        } else {
            PromptReason::Rejected
        };
        assert_eq!(req.reason, expected);
        self.asked.lock().unwrap().push(req.attempt);
        let next = self.answers.lock().unwrap().remove(0);
        next.map(|pw| Credentials {
            username: "alice".into(),
            password: SecretString::from(pw),
            save: false,
            method: AuthMethod::Basic,
        })
    }

    async fn confirm_certificate(&self, _: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

fn authed(answers: Vec<Option<&'static str>>) -> (FakeRaSession, Arc<Prompter>) {
    let pr = Arc::new(Prompter {
        answers: Mutex::new(answers),
        asked: Mutex::new(Vec::new()),
    });
    let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1")
        .expect("session")
        .with_required_auth("alice", "s3cret", Some(pr.clone()));
    (s, pr)
}

#[tokio::test]
async fn authentication_retries_then_succeeds_once() {
    let (s, pr) = authed(vec![Some("wrong"), Some("s3cret")]);
    let c = CancelToken::new();
    s.ensure_authenticated(&c).await.expect("auth");
    s.ensure_authenticated(&c)
        .await
        .expect("already authenticated, no new prompt");
    assert_eq!(*pr.asked.lock().unwrap(), vec![0, 1]);
}

#[tokio::test]
async fn authentication_failure_cancel_and_missing_prompter() {
    let (s, _) = authed(vec![Some("a"), Some("b"), Some("c")]);
    assert_eq!(
        s.ensure_authenticated(&CancelToken::new())
            .await
            .expect_err("fails")
            .code,
        ErrorCode::AuthRequired
    );

    let (s, _) = authed(vec![None]);
    assert_eq!(
        s.ensure_authenticated(&CancelToken::new())
            .await
            .expect_err("cancelled")
            .code,
        ErrorCode::Cancelled
    );

    let s = FakeRaSession::new("https://svn.test.local/r", "u")
        .expect("session")
        .with_required_auth("a", "b", None);
    assert_eq!(
        s.ensure_authenticated(&CancelToken::new())
            .await
            .expect_err("no prompter")
            .code,
        ErrorCode::AuthRequired
    );

    let open = FakeRaSession::new("https://svn.test.local/r", "u").expect("session");
    open.ensure_authenticated(&CancelToken::new())
        .await
        .expect("no auth required");
}

#[tokio::test]
async fn cancelled_token_stops_operations() {
    let s = session();
    let c = CancelToken::new();
    c.cancel();
    assert_eq!(
        s.youngest_rev(&c).await.expect_err("cancelled").code,
        ErrorCode::Cancelled
    );
    assert_eq!(
        s.ensure_authenticated(&c)
            .await
            .expect_err("cancelled")
            .code,
        ErrorCode::Cancelled
    );
    assert_eq!(
        s.list(&p(""), None, Depth::Immediates, &c)
            .await
            .expect_err("cancelled")
            .code,
        ErrorCode::Cancelled
    );
}

/// Observer whose journal can be told to fail.
struct Journal {
    fail_txn: bool,
    fail_merge: bool,
}

impl CommitObserver for Journal {
    fn on_txn_created(&self, _: &str) -> Result<()> {
        if self.fail_txn {
            Err(svn_types::SvnError::new(
                ErrorCode::DiskFull,
                "journal full",
            ))
        } else {
            Ok(())
        }
    }
    fn on_merged(&self, _: &CommitInfo) -> Result<()> {
        if self.fail_merge {
            Err(svn_types::SvnError::new(
                ErrorCode::PermissionDenied,
                "journal read-only",
            ))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn failing_journal_vetoes_the_commit_and_deletes_the_transaction() {
    let s = session();
    let obs = Arc::new(Journal {
        fail_txn: true,
        fail_merge: false,
    });
    let err = s
        .commit_editor("m", &[], &[], false, obs, &CancelToken::new())
        .await
        .err()
        .expect("vetoed");
    assert_eq!(err.code, ErrorCode::DiskFull);
    assert!(s.open_txns().is_empty(), "transaction must be deleted");
    assert_eq!(
        s.youngest_rev(&CancelToken::new()).await.expect("youngest"),
        Revnum(2)
    );
}

#[tokio::test]
async fn failing_merge_notification_is_reported_with_the_merged_revision() {
    let s = session();
    let obs = Arc::new(Journal {
        fail_txn: false,
        fail_merge: true,
    });
    let mut ed = s
        .commit_editor("m", &[], &[], false, obs, &CancelToken::new())
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    ed.add_directory(&p("newdir"), root, None)
        .await
        .expect("dir");
    let err = ed.close_edit().await.expect_err("observer failed");
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    assert!(err.message.contains("r3 was merged"), "{}", err.message);
    assert_eq!(
        s.youngest_rev(&CancelToken::new()).await.expect("youngest"),
        Revnum(3)
    );
}

#[tokio::test]
async fn cancelled_commit_stops_editing_and_can_still_be_aborted() {
    let s = session();
    let token = CancelToken::new();
    let mut ed = s
        .commit_editor("m", &[], &[], false, Arc::new(Recorder::default()), &token)
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    token.cancel();
    assert_eq!(
        ed.add_directory(&p("d"), root, None)
            .await
            .expect_err("cancelled")
            .code,
        ErrorCode::Cancelled
    );
    assert_eq!(
        ed.close_directory(root).await.expect_err("cancelled").code,
        ErrorCode::Cancelled
    );
    ed.abort_edit().await.expect("abort works after cancel");
    assert!(s.open_txns().is_empty());

    // Cancel just before the merge: no revision, no leaked transaction.
    let token = CancelToken::new();
    let ed = s
        .commit_editor("m", &[], &[], false, Arc::new(Recorder::default()), &token)
        .await
        .expect("editor");
    token.cancel();
    assert_eq!(
        ed.close_edit().await.expect_err("cancelled").code,
        ErrorCode::Cancelled
    );
    assert!(s.open_txns().is_empty());
    assert_eq!(
        s.youngest_rev(&CancelToken::new()).await.expect("youngest"),
        Revnum(2)
    );

    // An already cancelled token refuses to start a commit.
    let dead = CancelToken::new();
    dead.cancel();
    let err = s
        .commit_editor("m", &[], &[], false, Arc::new(Recorder::default()), &dead)
        .await
        .err()
        .expect("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
}

/// A FIFO makes the upload source block until a writer shows up, so the token can fire while
/// `apply_text` is awaiting its read.
#[cfg(unix)]
#[tokio::test]
async fn cancel_during_a_pending_read_is_reported_and_nothing_is_applied() {
    let s = session();
    let tmp = tempfile::tempdir().expect("tmp");
    let fifo = tmp.path().join("slow-source");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo")
            .success()
    );

    let token = CancelToken::new();
    let mut ed = s
        .commit_editor("m", &[], &[], false, Arc::new(Recorder::default()), &token)
        .await
        .expect("editor");
    let root = ed.open_root(None).await.expect("root");
    let f = ed.add_file(&p("late.txt"), root, None).await.expect("file");
    let src = ReplayableSource {
        path: fifo.clone(),
        translated: true,
        size: None,
    };
    let task = tokio::spawn(async move {
        let r = ed
            .apply_text(f, None, src, None, Checksum::Md5([0; 16]))
            .await;
        (ed, r)
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!task.is_finished(), "the read must still be pending");
    token.cancel();
    // Let the pending read complete after the cancellation.
    tokio::task::spawn_blocking(move || std::fs::write(&fifo, b"payload"))
        .await
        .expect("join")
        .expect("write fifo");

    let (ed, r) = task.await.expect("task");
    assert_eq!(
        r.expect_err("cancelled while reading").code,
        ErrorCode::Cancelled
    );
    ed.abort_edit().await.expect("abort");
    assert!(s.open_txns().is_empty());
    assert_eq!(
        s.check_path(&p("late.txt"), None, &CancelToken::new())
            .await
            .expect("kind"),
        NodeKind::None
    );
}
