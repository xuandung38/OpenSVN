//! Best-effort transaction cleanup of the commit editor over HTTPS: it never prompts (not
//! from `Drop`, `abort_edit` or a failed `close_edit`) and it gives up after its timeout.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Handler, Reply, ScriptedPrompter, TestCa, TlsServer, transport_trusting};
use reqwest::Method;
use svn_ra::ops::commit::{abort_txn, start_commit};
use svn_ra::ops::update::RaWire;
use svn_ra_api::{CommitEditor, CommitInfo, CommitObserver};
use svn_types::{CancelToken, ErrorCode, RepoUrl, Result};

const TXN: &str = "7-a";
const TXN_PATH: &str = "/svn/repo/!svn/txn/7-a";

struct NoObserver;

impl CommitObserver for NoObserver {
    fn on_txn_created(&self, _txn: &str) -> Result<()> {
        Ok(())
    }

    fn on_merged(&self, _info: &CommitInfo) -> Result<()> {
        Ok(())
    }
}

/// `OPTIONS` answer of an HTTPv2 server, as `mod_dav_svn` 1.14 sends it.
fn options() -> Reply {
    let mut r = Reply::status(200);
    for dav in [
        "1,2",
        "http://subversion.tigris.org/xmlns/dav/svn/depth",
        "http://subversion.tigris.org/xmlns/dav/svn/ephemeral-txnprops",
        "http://subversion.tigris.org/xmlns/dav/svn/svndiff1",
    ] {
        r = r.header("dav", dav);
    }
    for (name, value) in [
        ("svn-youngest-rev", "5"),
        (
            "svn-repository-uuid",
            "7462d98e-4071-4508-b797-d2922b5cda8f",
        ),
        ("svn-repository-root", "/svn/repo"),
        ("svn-me-resource", "/svn/repo/!svn/me"),
        ("svn-rev-root-stub", "/svn/repo/!svn/rvr"),
        ("svn-rev-stub", "/svn/repo/!svn/rev"),
        ("svn-txn-root-stub", "/svn/repo/!svn/txr"),
        ("svn-txn-stub", "/svn/repo/!svn/txn"),
        ("svn-vtxn-root-stub", "/svn/repo/!svn/vtxr"),
        ("svn-vtxn-stub", "/svn/repo/!svn/vtxn"),
        ("svn-supported-posts", "create-txn-with-props"),
    ] {
        r = r.header(name, value);
    }
    r
}

/// A server that lets the commit start without credentials and answers everything else
/// (the cleanup `DELETE` included) with `rest`.
async fn server(rest: impl Fn(&Method) -> Reply + Send + Sync + 'static) -> (TestCa, TlsServer) {
    let handler: Arc<Handler> = Arc::new(move |seen| match seen.method.as_str() {
        "OPTIONS" => options(),
        "POST" => Reply::status(201).header("svn-txn-name", TXN),
        _ => rest(&seen.method),
    });
    let ca = TestCa::new();
    let srv = TlsServer::start(&ca.leaf(&["localhost"], false), handler).await;
    (ca, srv)
}

async fn begin(
    ca: &TestCa,
    srv: &TlsServer,
    prompter: Arc<ScriptedPrompter>,
) -> (RaWire, Box<dyn CommitEditor>) {
    let transport = transport_trusting(ca, prompter, |_| {});
    let url = RepoUrl::parse(srv.url("/svn/repo/trunk").as_str()).unwrap();
    let wire = RaWire::open(transport, url, &CancelToken::new())
        .await
        .unwrap();
    let ed = start_commit(
        &wire,
        "msg",
        &[],
        &[],
        false,
        Arc::new(NoObserver),
        &CancelToken::new(),
    )
    .await
    .unwrap();
    (wire, ed)
}

fn deletes(srv: &TlsServer) -> usize {
    srv.requests()
        .iter()
        .filter(|s| s.method == Method::DELETE && s.path == TXN_PATH)
        .count()
}

/// Every `DELETE` asks for credentials the session does not have.
fn deletes_need_login(m: &Method) -> Reply {
    if *m == Method::DELETE {
        Reply::unauthorized("repo")
    } else {
        Reply::status(409)
            .header("content-type", "text/xml")
            .with_body(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:error xmlns:D=\"DAV:\" \
             xmlns:m=\"http://apache.org/dav/xmlns\" xmlns:C=\"svn:\"><C:error/>\
             <m:human-readable errcode=\"160028\">out of date</m:human-readable></D:error>",
            )
    }
}

trait WithBody {
    fn with_body(self, body: &'static str) -> Self;
}

impl WithBody for Reply {
    fn with_body(mut self, body: &'static str) -> Self {
        self.body = common::TestBody::Full(Some(bytes::Bytes::from_static(body.as_bytes())));
        self
    }
}

#[tokio::test]
async fn dropped_editor_cleanup_never_prompts() {
    let (ca, srv) = server(deletes_need_login).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let (wire, mut ed) = begin(&ca, &srv, prompter.clone()).await;
    ed.open_root(None).await.unwrap();
    drop(ed);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while deletes(&srv) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "transaction delete was not attempted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give a (wrong) prompt-and-resend path time to happen.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(prompter.cert_prompts(), 0);
    assert_eq!(
        deletes(&srv),
        1,
        "the 401 must not be retried with credentials"
    );

    // Control: the explicit, interactive reconcile against the same server does prompt.
    abort_txn(&wire, TXN).await.unwrap_err();
    assert!(prompter.credential_prompts() >= 1);
}

#[tokio::test]
async fn abort_edit_and_failed_close_edit_never_prompt() {
    let (ca, srv) = server(deletes_need_login).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");

    let (_wire, mut ed) = begin(&ca, &srv, prompter.clone()).await;
    ed.open_root(None).await.unwrap();
    let err = ed.abort_edit().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert_eq!(deletes(&srv), 1);

    // MERGE answers 409 out of date, so close_edit discards the transaction.
    let (_wire, mut ed) = begin(&ca, &srv, prompter.clone()).await;
    let root = ed.open_root(None).await.unwrap();
    ed.close_directory(root).await.unwrap();
    let err = ed.close_edit().await.unwrap_err();
    assert_eq!(err.code, ErrorCode::OutOfDate);
    assert_eq!(deletes(&srv), 2);

    assert_eq!(prompter.credential_prompts(), 0);
}

/// A cleanup `DELETE` the server never answers is abandoned after 15 s (virtual time).
#[tokio::test(flavor = "current_thread")]
async fn cleanup_gives_up_after_its_timeout() {
    let (ca, srv) = server(|m| {
        let mut r = Reply::status(204);
        if *m == Method::DELETE {
            // Longer than the cleanup timeout and the transport's read timeout (60 s): the
            // cleanup timeout must be what ends the wait.
            r.delay = Duration::from_secs(3600);
        }
        r
    })
    .await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let (_wire, mut ed) = begin(&ca, &srv, prompter.clone()).await;
    ed.open_root(None).await.unwrap();

    // From here on the clock only moves when the runtime is idle, straight to the next timer.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let real = std::time::Instant::now();
    let err = ed.abort_edit().await.unwrap_err();
    let waited = started.elapsed();

    assert_eq!(err.code, ErrorCode::Timeout, "{err:?}");
    assert!(
        waited >= Duration::from_secs(15) && waited < Duration::from_secs(16),
        "gave up after {waited:?}"
    );
    assert!(
        real.elapsed() < Duration::from_secs(10),
        "used the real clock"
    );
    assert_eq!(prompter.credential_prompts(), 0);
}
