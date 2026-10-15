//! Reads whose streamed body is cut off mid-way (#158): log, listing, file download, file
//! history and lock reports are asked for again on a fresh connection and resume after what the
//! consumer already has, or fail with the connection error when they cannot. Writes are never
//! sent twice.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use md5::Digest as _;
use svn_delta::WindowApplier;
use svn_ra::ops::HttpRaSession;
use svn_ra::transport::{
    HttpTransport, MemoryTrustStore, ProxyConfig, RetryPolicy, TransportConfig,
};
use svn_ra_api::{
    AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials, FileRev, FileRevHandler,
    LogRequest, RaSession,
};
use svn_types::{
    CancelToken, ConnectionFailure, Depth, ErrorCode, LogEntry, RelPath, RepoUrl, Result, Revnum,
    SvnError, TxDeltaWindow, WindowSink,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const ROOT: &str = "/svn/repo";

/// One request as the server read it.
#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    body: String,
}

/// What the server answers: a complete response, or one cut after `cut` body bytes and then
/// closed (the connection breaks mid-body).
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    cut: Option<usize>,
}

impl Reply {
    fn xml(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "text/xml; charset=\"utf-8\"",
            body: body.into(),
            cut: None,
        }
    }

    fn cut_at(mut self, at: usize) -> Self {
        assert!(at < self.body.len(), "cut {at} beyond {}", self.body.len());
        self.cut = Some(at);
        self
    }
}

type Handler = Arc<dyn Fn(&Req) -> Reply + Send + Sync>;

/// Plain HTTP server in front of a scripted `mod_dav_svn`: answers `OPTIONS` like a 1.15
/// server and everything else through `handler`, and records every other request.
struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    async fn start(handler: impl Fn(&Req) -> Reply + Send + Sync + 'static) -> Self {
        Self::start_cutting_options(0, handler).await
    }

    /// [`Self::start`] whose first `cut_options` `OPTIONS` answers are cut mid-body.
    async fn start_cutting_options(
        cut_options: usize,
        handler: impl Fn(&Req) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let handler: Handler = Arc::new(handler);
        let cut_options = Arc::new(AtomicUsize::new(cut_options));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Req>>> = Arc::default();
        let seen2 = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (handler, seen) = (Arc::clone(&handler), Arc::clone(&seen2));
                tokio::spawn(serve(tcp, handler, seen, Arc::clone(&cut_options)));
            }
        });
        Self { addr, seen }
    }

    fn url(&self) -> RepoUrl {
        RepoUrl::parse(&format!("http://{}{ROOT}/trunk", self.addr)).unwrap()
    }

    fn requests(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }

    fn count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method)
            .count()
    }
}

async fn serve(
    tcp: TcpStream,
    handler: Handler,
    seen: Arc<Mutex<Vec<Req>>>,
    cut_options: Arc<AtomicUsize>,
) {
    let mut io = BufReader::new(tcp);
    loop {
        let mut head = String::new();
        loop {
            let mut line = String::new();
            match io.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or_default().split(' ');
        let method = first.next().unwrap_or_default().to_owned();
        let path = first.next().unwrap_or_default().to_owned();
        let length = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if io.read_exact(&mut body).await.is_err() {
            return;
        }
        let req = Req {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        };
        let (reply, extra) = if req.method == "OPTIONS" {
            let reply = Reply::xml(200, OPTIONS_BODY);
            let cut = cut_options
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            (if cut { reply.cut_at(40) } else { reply }, OPTIONS_HEADERS)
        } else {
            seen.lock().unwrap().push(req.clone());
            (handler(&req), "")
        };
        let head = format!(
            "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\n{extra}\r\n",
            reply.status,
            reply.content_type,
            reply.body.len()
        );
        let tcp = io.get_mut();
        if tcp.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        match reply.cut {
            Some(at) => {
                let _ = tcp.write_all(&reply.body[..at]).await;
                let _ = tcp.flush().await;
                // A plain close in the middle of the body.
                return;
            }
            None => {
                if tcp.write_all(&reply.body).await.is_err() {
                    return;
                }
            }
        }
    }
}

const OPTIONS_BODY: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<D:options-response xmlns:D=\"DAV:\">\n\
<D:activity-collection-set><D:href>/svn/repo/!svn/act/</D:href></D:activity-collection-set>\
</D:options-response>\n";

const OPTIONS_HEADERS: &str = "DAV: 1,2\r\n\
DAV: http://subversion.tigris.org/xmlns/dav/svn/depth\r\n\
DAV: http://subversion.tigris.org/xmlns/dav/svn/log-revprops\r\n\
DAV: http://subversion.tigris.org/xmlns/dav/svn/inline-props\r\n\
DAV: http://subversion.tigris.org/xmlns/dav/svn/svndiff1\r\n\
SVN-Youngest-Rev: 9\r\n\
SVN-Repository-UUID: 7462d98e-4071-4508-b797-d2922b5cda8f\r\n\
SVN-Repository-Root: /svn/repo\r\n\
SVN-Me-Resource: /svn/repo/!svn/me\r\n\
SVN-Rev-Root-Stub: /svn/repo/!svn/rvr\r\n\
SVN-Rev-Stub: /svn/repo/!svn/rev\r\n\
SVN-Txn-Root-Stub: /svn/repo/!svn/txr\r\n\
SVN-Txn-Stub: /svn/repo/!svn/txn\r\n\
SVN-VTxn-Root-Stub: /svn/repo/!svn/vtxr\r\n\
SVN-VTxn-Stub: /svn/repo/!svn/vtxn\r\n\
SVN-Supported-Posts: create-txn\r\n\
SVN-Allow-Bulk-Updates: Prefer\r\n";

struct NoPrompts;

#[async_trait::async_trait]
impl AuthPrompter for NoPrompts {
    async fn credentials(&self, _req: &CredentialRequest) -> Option<Credentials> {
        None
    }
    async fn confirm_certificate(&self, _info: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

fn transport_with(delays: Vec<Duration>) -> Arc<HttpTransport> {
    let config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        retry: RetryPolicy { delays },
        ..TransportConfig::default()
    };
    Arc::new(HttpTransport::new(
        config,
        Arc::new(NoPrompts),
        Arc::new(MemoryTrustStore::new()),
    ))
}

async fn session_with(srv: &Server, delays: Vec<Duration>) -> HttpRaSession {
    HttpRaSession::open(transport_with(delays), srv.url(), &CancelToken::new())
        .await
        .expect("open session")
}

/// Three quick retries.
async fn session(srv: &Server) -> HttpRaSession {
    session_with(srv, vec![Duration::from_millis(10); 3]).await
}

fn p(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

fn tag(body: &str, name: &str) -> Option<u64> {
    let open = format!("<S:{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find('<')? + start;
    body[start..end].trim().parse().ok()
}

// ---- log ----

/// Revisions of the scripted history.
const HISTORY: [u64; 6] = [9, 7, 5, 4, 3, 2];

/// The `log-report` a server sends for the request body: the revisions of [`HISTORY`] in
/// the range, in its order, at most `limit`.
fn log_report(body: &str) -> Vec<u8> {
    let start = tag(body, "start-revision").unwrap();
    let end = tag(body, "end-revision").unwrap();
    let limit = tag(body, "limit").filter(|l| *l > 0);
    let (lo, hi) = (start.min(end), start.max(end));
    let mut revs: Vec<u64> = HISTORY
        .iter()
        .copied()
        .filter(|r| (lo..=hi).contains(r))
        .collect();
    if start < end {
        revs.reverse();
    }
    if let Some(l) = limit {
        revs.truncate(l as usize);
    }
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:log-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n",
    );
    for r in revs {
        out.push_str(&format!(
            "<S:log-item>\n<D:version-name>{r}</D:version-name>\n<D:comment>r{r}</D:comment>\n\
             <D:creator-displayname>alice</D:creator-displayname>\n</S:log-item>\n"
        ));
    }
    out.push_str("</S:log-report>\n");
    out.into_bytes()
}

/// Offset just after the `n`-th complete log item (`n == 0`: inside the XML prolog).
fn after_items(body: &[u8], n: usize) -> usize {
    if n == 0 {
        return 20;
    }
    let text = std::str::from_utf8(body).unwrap();
    let mut at = 0;
    for _ in 0..n {
        at += text[at..].find("</S:log-item>").unwrap() + "</S:log-item>".len();
    }
    // Into the next item, so the cut is mid-element.
    at + 12
}

/// A log server that cuts the `k`-th report (0-based) after `cuts[k]` complete items.
async fn log_server(cuts: Vec<Option<usize>>) -> Server {
    let n = AtomicUsize::new(0);
    Server::start(move |req| {
        assert_eq!(req.method, "REPORT");
        let body = log_report(&req.body);
        let k = n.fetch_add(1, Ordering::SeqCst);
        let reply = Reply::xml(200, body.clone());
        match cuts.get(k).copied().flatten() {
            Some(items) => reply.cut_at(after_items(&body, items)),
            None => reply,
        }
    })
    .await
}

async fn run_log(s: &HttpRaSession, req: &LogRequest) -> (Result<()>, Vec<u64>) {
    let mut seen = Vec::new();
    let r = s
        .log(
            req,
            &mut |e: LogEntry| {
                seen.push(e.rev.0);
                Ok(())
            },
            &CancelToken::new(),
        )
        .await;
    (r, seen)
}

fn log_req(start: u64, end: u64, limit: Option<u32>, strict: bool, paths: &[&str]) -> LogRequest {
    LogRequest {
        paths: paths.iter().map(|s| p(s)).collect(),
        start: Some(Revnum(start)),
        end: Some(Revnum(end)),
        limit,
        discover_changed_paths: false,
        strict_node_history: strict,
    }
}

fn starts(srv: &Server) -> Vec<(u64, Option<u64>)> {
    srv.requests()
        .iter()
        .map(|r| {
            (
                tag(&r.body, "start-revision").unwrap(),
                tag(&r.body, "limit"),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_log_cut_before_any_entry_is_asked_for_again() {
    let srv = log_server(vec![Some(0)]).await;
    let s = session(&srv).await;
    let (r, seen) = run_log(&s, &log_req(9, 1, None, false, &[])).await;
    r.unwrap();
    assert_eq!(seen, HISTORY);
    assert_eq!(starts(&srv), [(9, None), (9, None)]);
}

#[tokio::test]
async fn a_descending_log_without_copies_resumes_at_the_last_entry() {
    // Cut after 2 entries, then after 1 more.
    let srv = log_server(vec![Some(2), Some(2)]).await;
    let s = session(&srv).await;
    let (r, seen) = run_log(&s, &log_req(9, 1, Some(5), true, &["a.txt"])).await;
    r.unwrap();
    assert_eq!(seen, [9, 7, 5, 4, 3], "each entry once, the limit kept");
    // The start moves to the last entry handed over (which comes again and is skipped), and
    // the limit shrinks by the entries already handed over.
    assert_eq!(starts(&srv), [(9, Some(5)), (7, Some(4)), (5, Some(3))]);
    // The REPORT goes to the revision of its start, where the path exists.
    let paths: Vec<_> = srv.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/svn/repo/!svn/rvr/9/trunk/a.txt",
            "/svn/repo/!svn/rvr/7/trunk/a.txt",
            "/svn/repo/!svn/rvr/5/trunk/a.txt"
        ]
    );
}

#[tokio::test]
async fn a_descending_log_that_follows_copies_is_asked_again_from_its_start() {
    // With copies followed the path may have had another name at older revisions: the report
    // is asked for again as it was and the entries already handed over are skipped.
    let srv = log_server(vec![Some(3)]).await;
    let s = session(&srv).await;
    let (r, seen) = run_log(&s, &log_req(9, 1, Some(5), false, &["a.txt"])).await;
    r.unwrap();
    assert_eq!(seen, [9, 7, 5, 4, 3]);
    assert_eq!(starts(&srv), [(9, Some(5)), (9, Some(5))]);
}

#[tokio::test]
async fn an_ascending_log_resumes_after_the_last_entry() {
    let srv = log_server(vec![Some(2)]).await;
    let s = session(&srv).await;
    let (r, seen) = run_log(&s, &log_req(1, 9, Some(4), false, &[])).await;
    r.unwrap();
    assert_eq!(seen, [2, 3, 4, 5]);
    assert_eq!(starts(&srv), [(1, Some(4)), (4, Some(2))]);
}

#[tokio::test]
async fn a_log_cut_after_its_last_entry_is_complete() {
    // Cut inside the closing tags, after every entry of the range was handed over.
    let srv = Server::start(|req| {
        let body = log_report(&req.body);
        let at = body.len() - 5;
        Reply::xml(200, body).cut_at(at)
    })
    .await;
    let s = session(&srv).await;
    let (r, seen) = run_log(&s, &log_req(9, 2, None, false, &[])).await;
    r.unwrap();
    assert_eq!(seen, HISTORY);
    assert_eq!(srv.count("REPORT"), 1, "nothing can be left after r2");
}

#[tokio::test]
async fn a_log_that_keeps_breaking_without_progress_fails_with_the_connection_error() {
    // Every report breaks after the same two entries: the resumes hand over one more entry,
    // then nothing new; the retry budget (3 in a row without progress) ends it.
    let srv = Server::start(|req| {
        let body = log_report(&req.body);
        let at = after_items(&body, 1);
        Reply::xml(200, body).cut_at(at)
    })
    .await;
    let s = session(&srv).await;
    let (r, seen) = run_log(&s, &log_req(9, 1, None, false, &[])).await;
    let err = r.unwrap_err();
    assert!(err.connection_failure().is_some(), "{err:?}");
    assert_ne!(err.code, ErrorCode::Cancelled);
    // Only the first entry ever arrived; it was handed over once.
    assert_eq!(seen, [9]);
    assert_eq!(srv.count("REPORT"), 4);
}

#[tokio::test]
async fn a_consumer_error_is_never_retried_even_when_it_looks_like_a_connection_failure() {
    let srv = log_server(vec![]).await;
    let s = session(&srv).await;
    let mut calls = 0;
    let err = s
        .log(
            &log_req(9, 1, None, false, &[]),
            &mut |_| {
                calls += 1;
                Err(
                    SvnError::new(ErrorCode::Network, "the consumer's own upload")
                        .with_connection(ConnectionFailure::ConnectionReset),
                )
            },
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.message, "the consumer's own upload");
    assert_eq!(calls, 1);
    assert_eq!(srv.count("REPORT"), 1);
}

#[tokio::test]
async fn cancel_ends_the_wait_before_a_resume() {
    let srv = log_server(vec![Some(1)]).await;
    // A long delay before the re-request.
    let s = session_with(&srv, vec![Duration::from_secs(30)]).await;
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let started = Instant::now();
    let mut seen = Vec::new();
    let err = s
        .log(
            &log_req(9, 1, None, false, &[]),
            &mut |e| {
                seen.push(e.rev.0);
                Ok(())
            },
            &cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(srv.count("REPORT"), 1);
    assert_eq!(seen, [9]);
}

// ---- file download ----

const FILE_PATH: &str = "/svn/repo/!svn/rvr/9/trunk/a.txt";

fn file_content() -> Vec<u8> {
    (0..100_000u32).map(|i| (i % 251) as u8).collect()
}

fn allprop_for(content: &[u8]) -> Vec<u8> {
    let md5 = hex(&md5::Md5::digest(content));
    let base = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../svn-protocol/tests/data/propfind/allprop-file.resp"),
    )
    .unwrap();
    String::from_utf8(base)
        .unwrap()
        .replace("/svn/repo/!svn/rvr/5/trunk/a.txt", FILE_PATH)
        .replace("3e079718d4006e9d757934ce54439418", &md5)
        .into_bytes()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A file server whose `k`-th GET (0-based) is cut after `cuts[k]` bytes.
async fn file_server(content: Vec<u8>, cuts: Vec<Option<usize>>) -> Server {
    let props = allprop_for(&content);
    let n = AtomicUsize::new(0);
    Server::start(move |req| match req.method.as_str() {
        "PROPFIND" => Reply::xml(207, props.clone()),
        "GET" => {
            assert_eq!(req.path, FILE_PATH);
            let reply = Reply {
                status: 200,
                content_type: "text/plain",
                body: content.clone(),
                cut: None,
            };
            match cuts
                .get(n.fetch_add(1, Ordering::SeqCst))
                .copied()
                .flatten()
            {
                Some(at) => reply.cut_at(at),
                None => reply,
            }
        }
        m => panic!("unexpected {m}"),
    })
    .await
}

/// Sink that records how often it was written to.
#[derive(Default)]
struct Recording {
    data: Vec<u8>,
}

impl tokio::io::AsyncWrite for Recording {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.data.extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_download_cut_twice_continues_after_the_bytes_already_written() {
    let content = file_content();
    let srv = file_server(content.clone(), vec![Some(30_000), Some(70_000)]).await;
    let s = session(&srv).await;
    let mut sink = Recording::default();
    let meta = s
        .get_file(&p("a.txt"), Some(Revnum(9)), &mut sink, &CancelToken::new())
        .await
        .expect("download resumed twice");
    // Every byte once, in order, and the server's MD5 checked over the whole.
    assert_eq!(sink.data.len(), content.len());
    assert!(sink.data == content);
    assert!(meta.verified);
    assert_eq!(srv.count("GET"), 3);
}

#[tokio::test]
async fn a_download_that_keeps_breaking_at_the_same_byte_fails_with_the_connection_error() {
    let content = file_content();
    let srv = file_server(content.clone(), vec![Some(30_000); 10]).await;
    let s = session(&srv).await;
    let mut sink = Recording::default();
    let err = s
        .get_file(&p("a.txt"), Some(Revnum(9)), &mut sink, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(err.connection_failure().is_some(), "{err:?}");
    // The first cut, then three re-requests that brought nothing new.
    assert_eq!(srv.count("GET"), 4);
    assert_eq!(sink.data, content[..30_000]);
}

/// A file server announcing `props`, whose `k`-th GET (0-based) answers `gets[k]`: a body and
/// where it is cut.
async fn file_server_serving(props: Vec<u8>, gets: Vec<(Vec<u8>, Option<usize>)>) -> Server {
    let n = AtomicUsize::new(0);
    Server::start(move |req| match req.method.as_str() {
        "PROPFIND" => Reply::xml(207, props.clone()),
        "GET" => {
            let (body, cut) = gets[n.fetch_add(1, Ordering::SeqCst)].clone();
            let reply = Reply {
                status: 200,
                content_type: "text/plain",
                body,
                cut: None,
            };
            match cut {
                Some(at) => reply.cut_at(at),
                None => reply,
            }
        }
        m => panic!("unexpected {m}"),
    })
    .await
}

#[tokio::test]
async fn a_download_whose_content_differs_when_asked_again_fails_and_never_mixes() {
    let content = file_content();
    let mut other = content.clone();
    other[50_000] ^= 0xff;
    let srv = file_server_serving(
        allprop_for(&content),
        vec![(content.clone(), Some(30_000)), (other, None)],
    )
    .await;
    let s = session(&srv).await;
    let mut sink = Recording::default();
    let err = s
        .get_file(&p("a.txt"), Some(Revnum(9)), &mut sink, &CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupt, "{err:?}");
    assert_eq!(srv.count("GET"), 2);
}

#[tokio::test]
async fn a_download_without_the_servers_checksum_is_not_resumed() {
    let content = file_content();
    let props = String::from_utf8(allprop_for(&content))
        .unwrap()
        .lines()
        .filter(|l| !l.contains("md5-checksum"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    let srv = file_server_serving(
        props,
        vec![(content.clone(), Some(30_000)), (content.clone(), None)],
    )
    .await;
    let s = session(&srv).await;
    let mut sink = Recording::default();
    let err = s
        .get_file(&p("a.txt"), Some(Revnum(9)), &mut sink, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(err.connection_failure().is_some(), "{err:?}");
    assert!(err.message.contains("cannot be resumed"), "{err:?}");
    assert_eq!(srv.count("GET"), 1);
}

#[tokio::test]
async fn a_download_without_the_servers_checksum_cut_before_any_byte_is_asked_again() {
    let content = file_content();
    let props = String::from_utf8(allprop_for(&content))
        .unwrap()
        .lines()
        .filter(|l| !l.contains("md5-checksum"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    let srv = file_server_serving(
        props,
        vec![(content.clone(), Some(0)), (content.clone(), None)],
    )
    .await;
    let s = session(&srv).await;
    let mut sink = Recording::default();
    let meta = s
        .get_file(&p("a.txt"), Some(Revnum(9)), &mut sink, &CancelToken::new())
        .await
        .expect("nothing was written before the cut");
    assert!(!meta.verified);
    assert!(sink.data == content);
    assert_eq!(srv.count("GET"), 2);
}

#[tokio::test]
async fn a_download_is_not_retried_without_a_retry_budget() {
    let content = file_content();
    let srv = file_server(content, vec![Some(10)]).await;
    let s = session_with(&srv, Vec::new()).await;
    let mut sink = Recording::default();
    let err = s
        .get_file(&p("a.txt"), Some(Revnum(9)), &mut sink, &CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        err.connection_failure(),
        Some(ConnectionFailure::ConnectionClosedByServer),
        "{err:?}"
    );
    assert_eq!(srv.count("GET"), 1);
}

// ---- listing and locks: nothing reaches the caller before the whole answer ----

fn listing() -> Vec<u8> {
    let rvr = "/svn/repo/!svn/rvr/9";
    let mut out =
        String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">");
    let mut entry = |href: String, dir: bool| {
        let rt = if dir { "<D:collection/>" } else { "" };
        out.push_str(&format!(
            "<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
             <D:resourcetype>{rt}</D:resourcetype><D:getcontentlength>4</D:getcontentlength>\
             <D:version-name>9</D:version-name>\
             <D:creator-displayname>alice</D:creator-displayname>\
             <D:creationdate>2026-10-13T09:45:07.190780Z</D:creationdate>\
             </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
        ));
    };
    entry(format!("{rvr}/trunk/"), true);
    for i in 0..50 {
        entry(format!("{rvr}/trunk/f{i:02}.txt"), false);
    }
    out.push_str("</D:multistatus>");
    out.into_bytes()
}

#[tokio::test]
async fn a_listing_cut_mid_way_is_asked_for_again_from_the_start() {
    let n = AtomicUsize::new(0);
    let srv = Server::start(move |req| {
        assert_eq!(req.method, "PROPFIND");
        let body = listing();
        if n.fetch_add(1, Ordering::SeqCst) == 0 {
            let at = body.len() / 2;
            Reply::xml(207, body).cut_at(at)
        } else {
            Reply::xml(207, body)
        }
    })
    .await;
    let s = session(&srv).await;
    let entries = s
        .list(
            &RelPath::root(),
            Some(Revnum(9)),
            Depth::Immediates,
            &CancelToken::new(),
        )
        .await
        .expect("listing asked for again");
    assert_eq!(entries.len(), 50);
    let names: std::collections::HashSet<_> = entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 50, "no entry twice");
    assert_eq!(srv.count("PROPFIND"), 2);
}

#[tokio::test]
async fn a_lock_report_cut_mid_way_is_asked_for_again() {
    let body = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../svn-protocol/tests/data/locks/get-locks-report.resp"),
    )
    .unwrap();
    let n = AtomicUsize::new(0);
    let srv = Server::start(move |req| {
        assert_eq!(req.method, "REPORT");
        if n.fetch_add(1, Ordering::SeqCst) < 2 {
            let at = body.len() / 2;
            Reply::xml(200, body.clone()).cut_at(at)
        } else {
            Reply::xml(200, body.clone())
        }
    })
    .await;
    let s = session(&srv).await;
    let locks = s
        .get_locks(&RelPath::root(), Depth::Infinity, &CancelToken::new())
        .await
        .expect("report asked for again");
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].path.as_str(), "/trunk/a.txt");
    assert_eq!(srv.count("REPORT"), 3);
}

// ---- file history (blame) ----

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Applier(WindowApplier<Cursor<Vec<u8>>, Shared>);

impl WindowSink for Applier {
    fn window(&mut self, w: Option<&TxDeltaWindow>) -> Result<()> {
        match w {
            Some(w) => self.0.apply_window(w),
            None => self.0.finish().map(|_| ()),
        }
    }
}

/// Rebuilds every revision from the previous one, like blame.
#[derive(Default)]
struct Rebuild {
    revs: Vec<u64>,
    texts: Vec<Shared>,
}

impl FileRevHandler for Rebuild {
    fn revision(&mut self, rev: FileRev) -> Result<Option<Box<dyn WindowSink>>> {
        let base = self
            .texts
            .last()
            .map(|t| t.0.lock().unwrap().clone())
            .unwrap_or_default();
        let out = Shared::default();
        self.texts.push(out.clone());
        self.revs.push(rev.rev.0);
        Ok(Some(Box::new(Applier(WindowApplier::new(
            Cursor::new(base),
            out,
        )))))
    }
}

fn blame_report() -> Vec<u8> {
    std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../svn-protocol/tests/data/file_revs/blame.resp"),
    )
    .unwrap()
}

/// Offset in the blame report just after the `n`-th `</S:file-rev>`, or (with `inside`) in the
/// middle of the `n+1`-th delta.
fn blame_cut(n: usize, inside: bool) -> usize {
    let body = blame_report();
    let text = std::str::from_utf8(&body).unwrap();
    let mut at = 0;
    for _ in 0..n {
        at += text[at..].find("</S:file-rev>").unwrap() + "</S:file-rev>".len();
    }
    if inside {
        at + text[at..].find("<S:txdelta>").unwrap() + "<S:txdelta>".len() + 4
    } else {
        at + 3
    }
}

async fn blame_server(cuts: Vec<Option<usize>>) -> Server {
    let n = AtomicUsize::new(0);
    Server::start(move |req| {
        assert_eq!(req.method, "REPORT");
        assert_eq!(req.path, "/svn/repo/!svn/rvr/9/trunk/a.txt");
        let reply = Reply::xml(200, blame_report());
        match cuts
            .get(n.fetch_add(1, Ordering::SeqCst))
            .copied()
            .flatten()
        {
            Some(at) => reply.cut_at(at),
            None => reply,
        }
    })
    .await
}

async fn reference_texts() -> Vec<Vec<u8>> {
    let srv = blame_server(vec![]).await;
    let s = session(&srv).await;
    let mut h = Rebuild::default();
    s.file_revs(
        &p("a.txt"),
        None,
        Some(Revnum(9)),
        &mut h,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    h.texts
        .iter()
        .map(|t| t.0.lock().unwrap().clone())
        .collect()
}

#[tokio::test]
async fn a_file_history_cut_between_revisions_resumes_after_the_ones_handed_over() {
    let want = reference_texts().await;
    let srv = blame_server(vec![Some(blame_cut(2, false)), Some(blame_cut(3, false))]).await;
    let s = session(&srv).await;
    let mut h = Rebuild::default();
    s.file_revs(
        &p("a.txt"),
        None,
        Some(Revnum(9)),
        &mut h,
        &CancelToken::new(),
    )
    .await
    .expect("history resumed twice");
    assert_eq!(h.revs, [2, 3, 6, 9], "each revision once");
    let got: Vec<Vec<u8>> = h
        .texts
        .iter()
        .map(|t| t.0.lock().unwrap().clone())
        .collect();
    assert_eq!(got, want, "every revision rebuilt as without the cuts");
    assert_eq!(srv.count("REPORT"), 3);
}

#[tokio::test]
async fn a_file_history_cut_inside_a_delta_the_handler_receives_fails_as_incomplete() {
    let srv = blame_server(vec![Some(blame_cut(1, true))]).await;
    let s = session(&srv).await;
    let mut h = Rebuild::default();
    let err = s
        .file_revs(
            &p("a.txt"),
            None,
            Some(Revnum(9)),
            &mut h,
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.connection_failure(),
        Some(ConnectionFailure::ConnectionClosedByServer),
        "{err:?}"
    );
    assert!(err.message.contains("cannot be resumed"), "{}", err.message);
    assert_eq!(srv.count("REPORT"), 1);
}

#[tokio::test]
async fn a_file_history_cut_before_any_revision_is_asked_for_again() {
    let srv = blame_server(vec![Some(30)]).await;
    let s = session(&srv).await;
    let mut h = Rebuild::default();
    s.file_revs(
        &p("a.txt"),
        None,
        Some(Revnum(9)),
        &mut h,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(h.revs, [2, 3, 6, 9]);
    assert_eq!(srv.count("REPORT"), 2);
}

// ---- writes ----

#[tokio::test]
async fn a_lock_whose_answer_is_cut_is_not_sent_again() {
    let body = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../svn-protocol/tests/data/locks/lock-ok.resp"),
    )
    .unwrap();
    let srv = Server::start(move |req| {
        assert_eq!(req.method, "LOCK");
        let at = body.len() / 2;
        Reply::xml(200, body.clone()).cut_at(at)
    })
    .await;
    let s = session(&srv).await;
    let err = s
        .lock(&p("a.txt"), None, false, None, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(err.connection_failure().is_some(), "{err:?}");
    assert_eq!(srv.count("LOCK"), 1);
}

// ---- session open ----

#[tokio::test]
async fn an_options_answer_cut_mid_body_is_asked_for_again() {
    let srv = Server::start_cutting_options(2, |req| panic!("unexpected {}", req.method)).await;
    let s = session(&srv).await;
    assert_eq!(s.uuid(), "7462d98e-4071-4508-b797-d2922b5cda8f");
}

#[tokio::test]
async fn an_options_answer_that_keeps_breaking_fails_with_the_connection_error() {
    let srv = Server::start_cutting_options(10, |req| panic!("unexpected {}", req.method)).await;
    let Err(err) = HttpRaSession::open(
        transport_with(vec![Duration::from_millis(10); 3]),
        srv.url(),
        &CancelToken::new(),
    )
    .await
    else {
        panic!("every answer cut");
    };
    assert!(err.connection_failure().is_some(), "{err:?}");
}
