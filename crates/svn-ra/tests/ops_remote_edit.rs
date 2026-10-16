//! Remote edits (mkdir, delete, copy, move) as single commits.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod ra_write_support;

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use ra_write_support::{Bulk, ROOT, fixture, requests, server, wire};
use svn_ra::ops::commit::start_commit;
use svn_ra::ops::remote_edit::{RemoteCommit, copy, delete, mkdir, move_path};
use svn_ra::ops::update::RaWire;
use svn_ra_api::fake::{FakeChange, FakeRaSession};
use svn_ra_api::{
    CommitEditor, CommitInfo, CommitObserver, FileMeta, FileRevHandler, LogRequest, RaSession,
    ReportFn, UpdateRequest,
};
use svn_types::{
    CancelToken, DeltaEditor, Depth, DirEntry, ErrorCode, LockInfo, LogEntry, NodeKind, PropChange,
    RelPath, RepoUrl, Result, Revnum, SvnError,
};
use tokio::io::AsyncWrite;
use wiremock::matchers::method;
use wiremock::{Mock, ResponseTemplate};

fn p(s: &str) -> RelPath {
    RelPath::new(s).expect("path")
}

struct NoObserver;

impl CommitObserver for NoObserver {
    fn on_txn_created(&self, _txn: &str) -> Result<()> {
        Ok(())
    }
    fn on_merged(&self, _info: &CommitInfo) -> Result<()> {
        Ok(())
    }
}

fn commit() -> RemoteCommit {
    RemoteCommit {
        message: "remote edit".into(),
        rev_props: Vec::new(),
        observer: Arc::new(NoObserver),
    }
}

fn seeded() -> FakeRaSession {
    let s = FakeRaSession::new("https://svn.test.local/repos/demo", "uuid-1").unwrap();
    s.seed(
        "alice",
        "first",
        &[
            FakeChange::PutFile(p("trunk/a.txt"), Bytes::from_static(b"one")),
            FakeChange::PutDir(p("branches")),
        ],
    )
    .unwrap();
    s
}

#[tokio::test]
async fn mkdir_creates_all_directories_in_one_revision() {
    let s = seeded();
    let c = CancelToken::new();
    let before = s.youngest_rev(&c).await.unwrap();
    let info = mkdir(
        &s,
        &[p("trunk/n1"), p("branches/b"), p("trunk/n2")],
        &commit(),
        &c,
    )
    .await
    .unwrap();
    assert_eq!(info.revision.0, before.0 + 1);
    for d in ["trunk/n1", "trunk/n2", "branches/b"] {
        assert_eq!(
            s.check_path(&p(d), None, &c).await.unwrap(),
            NodeKind::Dir,
            "{d}"
        );
    }
}

#[tokio::test]
async fn delete_removes_nodes() {
    let s = seeded();
    let c = CancelToken::new();
    delete(&s, &[p("trunk/a.txt")], &commit(), &c)
        .await
        .unwrap();
    assert_eq!(
        s.check_path(&p("trunk/a.txt"), None, &c).await.unwrap(),
        NodeKind::None
    );
}

#[tokio::test]
async fn failed_edit_leaves_no_revision_and_no_transaction() {
    let s = seeded();
    let c = CancelToken::new();
    let before = s.youngest_rev(&c).await.unwrap();
    // `trunk/a.txt` exists already.
    let err = mkdir(&s, &[p("trunk/a.txt")], &commit(), &c)
        .await
        .unwrap_err();
    assert_ne!(err.code, ErrorCode::Cancelled);
    assert_eq!(s.youngest_rev(&c).await.unwrap(), before);
    assert!(s.open_txns().is_empty());
}

#[tokio::test]
async fn root_and_empty_requests_are_refused() {
    let s = seeded();
    let c = CancelToken::new();
    assert!(mkdir(&s, &[RelPath::root()], &commit(), &c).await.is_err());
    assert!(mkdir(&s, &[], &commit(), &c).await.is_err());
    assert!(
        move_path(&s, &p("trunk"), &p("trunk/inside"), &commit(), &c)
            .await
            .is_err()
    );
}

/// Session whose commits go over HTTP; reads are answered with fixed values.
struct HttpSession {
    wire: RaWire,
    root: RepoUrl,
}

fn unsupported<T>() -> Result<T> {
    Err(SvnError::new(
        ErrorCode::Internal,
        "not needed by remote edits",
    ))
}

#[async_trait]
impl RaSession for HttpSession {
    fn repos_root(&self) -> &RepoUrl {
        &self.root
    }
    fn uuid(&self) -> &str {
        "uuid"
    }
    fn session_url(&self) -> &RepoUrl {
        self.wire.session_url()
    }
    async fn ensure_authenticated(&self, _c: &CancelToken) -> Result<()> {
        Ok(())
    }
    async fn youngest_rev(&self, _c: &CancelToken) -> Result<Revnum> {
        Ok(Revnum(5))
    }
    async fn check_path(
        &self,
        path: &RelPath,
        _r: Option<Revnum>,
        _c: &CancelToken,
    ) -> Result<NodeKind> {
        Ok(if path.as_str().ends_with(".txt") {
            NodeKind::File
        } else {
            NodeKind::Dir
        })
    }
    async fn stat(
        &self,
        _p: &RelPath,
        _r: Option<Revnum>,
        _c: &CancelToken,
    ) -> Result<Option<DirEntry>> {
        unsupported()
    }
    async fn list(
        &self,
        _p: &RelPath,
        _r: Option<Revnum>,
        _d: Depth,
        _c: &CancelToken,
    ) -> Result<Vec<DirEntry>> {
        unsupported()
    }
    async fn get_file(
        &self,
        _p: &RelPath,
        _r: Option<Revnum>,
        _s: &mut (dyn AsyncWrite + Send + Unpin),
        _c: &CancelToken,
    ) -> Result<FileMeta> {
        unsupported()
    }
    async fn log(
        &self,
        _r: &LogRequest,
        _f: &mut (dyn FnMut(LogEntry) -> Result<()> + Send),
        _c: &CancelToken,
    ) -> Result<()> {
        unsupported()
    }
    async fn get_locks(&self, _p: &RelPath, _d: Depth, _c: &CancelToken) -> Result<Vec<LockInfo>> {
        unsupported()
    }
    async fn lock(
        &self,
        _p: &RelPath,
        _m: Option<&str>,
        _s: bool,
        _b: Option<Revnum>,
        _c: &CancelToken,
    ) -> Result<LockInfo> {
        unsupported()
    }
    async fn unlock(
        &self,
        _p: &RelPath,
        _t: Option<&str>,
        _b: bool,
        _c: &CancelToken,
    ) -> Result<()> {
        unsupported()
    }
    async fn file_revs(
        &self,
        _p: &RelPath,
        _s: Option<Revnum>,
        _e: Option<Revnum>,
        _h: &mut dyn FileRevHandler,
        _c: &CancelToken,
    ) -> Result<()> {
        unsupported()
    }
    async fn get_location_segments(
        &self,
        _p: &RelPath,
        _peg: Option<Revnum>,
        _s: Option<Revnum>,
        _e: Option<Revnum>,
        _c: &CancelToken,
    ) -> Result<Vec<svn_ra_api::LocationSegment>> {
        unsupported()
    }
    async fn do_update(
        &self,
        _r: UpdateRequest,
        _f: ReportFn,
        _e: Box<dyn DeltaEditor>,
        _c: &CancelToken,
    ) -> Result<()> {
        unsupported()
    }
    async fn commit_editor(
        &self,
        message: &str,
        rev_props: &[PropChange],
        lock_tokens: &[(String, String)],
        keep_locks: bool,
        observer: Arc<dyn CommitObserver>,
        cancel: &CancelToken,
    ) -> Result<Box<dyn CommitEditor>> {
        start_commit(
            &self.wire,
            message,
            rev_props,
            lock_tokens,
            keep_locks,
            observer,
            cancel,
        )
        .await
    }
    async fn abort_txn(&self, _t: &str) -> Result<()> {
        Ok(())
    }
}

async fn http_session() -> (wiremock::MockServer, HttpSession) {
    let srv = server(Bulk::Prefer).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).append_header("SVN-Txn-Name", "3-c"))
        .mount(&srv)
        .await;
    for (m, s) in [("COPY", 201), ("DELETE", 204)] {
        Mock::given(method(m))
            .respond_with(ResponseTemplate::new(s))
            .mount(&srv)
            .await;
    }
    Mock::given(method("MERGE"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture("commit/full-merge.resp")))
        .mount(&srv)
        .await;
    let w = wire(&srv, 4).await;
    let root = RepoUrl::parse(&format!("{}{ROOT}", srv.uri())).unwrap();
    (srv, HttpSession { wire: w, root })
}

#[tokio::test]
async fn copy_over_http_is_one_server_side_copy() {
    let (srv, s) = http_session().await;
    let c = CancelToken::new();
    let info = copy(&s, &p("src"), Some(Revnum(4)), &p("dst"), &commit(), &c)
        .await
        .unwrap();
    assert_eq!(info.revision, Revnum(10));
    let reqs = requests(&srv).await;
    let seq: Vec<_> = reqs.iter().map(|(m, p)| (m.as_str(), p.as_str())).collect();
    assert_eq!(
        seq,
        [
            ("POST", "/svn/repo/!svn/me"),
            ("COPY", "/svn/repo/!svn/rvr/4/trunk/src"),
            ("MERGE", "/svn/repo/trunk"),
        ]
    );
}

#[tokio::test]
async fn move_over_http_copies_then_deletes_at_the_source_revision() {
    let (srv, s) = http_session().await;
    let c = CancelToken::new();
    move_path(&s, &p("old.txt"), &p("new.txt"), &commit(), &c)
        .await
        .unwrap();
    let all = srv.received_requests().await.unwrap();
    let del = all.iter().find(|r| r.method.as_str() == "DELETE").unwrap();
    assert_eq!(del.url.path(), "/svn/repo/!svn/txr/3-c/trunk/old.txt");
    assert_eq!(del.headers.get("X-SVN-Version-Name").unwrap(), "5");
    let copy = all.iter().find(|r| r.method.as_str() == "COPY").unwrap();
    assert_eq!(copy.url.path(), "/svn/repo/!svn/rvr/5/trunk/old.txt");
    assert!(
        copy.headers.get("Depth").is_none(),
        "files are copied without Depth"
    );
}
