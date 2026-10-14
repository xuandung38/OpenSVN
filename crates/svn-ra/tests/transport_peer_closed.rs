//! Connections that the server or a proxy closes, resets or cuts off, as Apache on Windows, IIS
//! and many proxies do: TLS ended without `close_notify`, keep-alive connections closed or reset
//! under the next request, bodies cut off mid-way. Reads are sent again on a fresh connection;
//! writes are never sent twice.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{ScriptedPrompter, TestCa};
use reqwest::Method;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use svn_ra::transport::{HttpRequest, HttpTransport, RequestBody, RetryPolicy, is_idempotent};
use svn_types::{CancelToken, ConnectionFailure, ErrorCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// What the server does with one request.
#[derive(Debug, Clone)]
enum Act {
    /// Complete response with `Content-Length`; the connection stays open.
    Ok(String),
    /// Complete response with `Content-Length`, then TCP close without `close_notify`.
    OkThenUncleanClose(String),
    /// Response headers announcing `body.len()` bytes, `keep` of them, then a close without
    /// `close_notify`.
    Truncate { body: String, keep: usize },
    /// Chunked response cut after its first chunk, then a close without `close_notify`.
    TruncateChunked,
    /// No answer: close without `close_notify`.
    CloseUnclean,
    /// No answer: TCP reset.
    Reset,
    /// Headers, then nothing (the body stalls).
    Stall,
}

/// Raw HTTPS server that answers each request with the next scripted [`Act`] (`Ok("ok")` once
/// the script is used up) and records the method of every request it read.
struct RawServer {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    connections: Arc<Mutex<usize>>,
}

impl RawServer {
    async fn start(ca: &TestCa, script: Vec<Act>) -> Self {
        Self::start_with(ca, script, false).await
    }

    /// With `close_reused`, every request after the first one of a connection is answered by
    /// closing that connection without `close_notify` (a server that ends keep-alive
    /// connections under the next request).
    async fn start_with(ca: &TestCa, script: Vec<Act>, close_reused: bool) -> Self {
        let leaf = ca.leaf(&["localhost"], false);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.cert_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key_der.clone())),
            )
            .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let connections: Arc<Mutex<usize>> = Arc::default();
        let (seen2, conns2) = (Arc::clone(&seen), Arc::clone(&connections));
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                *conns2.lock().unwrap() += 1;
                let (acceptor, script, seen) =
                    (acceptor.clone(), Arc::clone(&script), Arc::clone(&seen2));
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    serve_connection(tls, &script, &seen, close_reused).await;
                });
            }
        });
        Self {
            addr,
            seen,
            connections,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}{path}", self.addr.port())
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        *self.connections.lock().unwrap()
    }
}

/// Reads one request (headers plus a `Content-Length` body); `None` at end of stream.
async fn read_request(tls: &mut TlsStream<TcpStream>) -> Option<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match tls.read(&mut byte).await {
            Ok(1) => buf.push(byte[0]),
            _ => return None,
        }
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    tls.read_exact(&mut body).await.ok()?;
    head.split(' ').next().map(str::to_owned)
}

/// Ends the connection without a TLS `close_notify`: the TLS records are flushed, then the TCP
/// stream is dropped (FIN).
async fn unclean_close(mut tls: TlsStream<TcpStream>) {
    let _ = tls.flush().await;
    drop(tls);
}

async fn serve_connection(
    mut tls: TlsStream<TcpStream>,
    script: &Mutex<VecDeque<Act>>,
    seen: &Mutex<Vec<String>>,
    close_reused: bool,
) {
    let mut served = 0usize;
    loop {
        let Some(method) = read_request(&mut tls).await else {
            return;
        };
        seen.lock().unwrap().push(method);
        served += 1;
        if close_reused && served > 1 {
            return unclean_close(tls).await;
        }
        let act = script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Act::Ok("ok".into()));
        match act {
            Act::Ok(body) => {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                if tls.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
            }
            Act::OkThenUncleanClose(body) => {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                return unclean_close(tls).await;
            }
            Act::Truncate { body, keep } => {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    &body[..keep]
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                return unclean_close(tls).await;
            }
            Act::TruncateChunked => {
                let resp = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n";
                let _ = tls.write_all(resp.as_bytes()).await;
                return unclean_close(tls).await;
            }
            Act::CloseUnclean => return unclean_close(tls).await,
            Act::Reset => {
                // SO_LINGER 0: the close sends an RST instead of a FIN.
                let tcp = tls.into_inner().0.into_std().unwrap();
                socket_reset(tcp);
                return;
            }
            Act::Stall => {
                let _ = tls
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc")
                    .await;
                let _ = tls.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
        }
    }
}

/// Closes `tcp` with SO_LINGER 0, so the peer gets an RST instead of a FIN. (Deprecated because a
/// non-zero linger blocks the thread on drop; zero does not.)
#[allow(deprecated)]
fn socket_reset(tcp: std::net::TcpStream) {
    let sock = tokio::net::TcpSocket::from_std_stream(tcp);
    sock.set_linger(Some(Duration::ZERO)).unwrap();
    drop(sock);
}

const SLOW_RETRY: Duration = Duration::from_secs(5);

/// Transport trusting `ca`, with three retries after [`SLOW_RETRY`] each, so a test can tell an
/// immediate retry from a delayed one.
fn transport(
    ca: &TestCa,
    tweak: impl FnOnce(&mut svn_ra::transport::TransportConfig),
) -> Arc<HttpTransport> {
    common::transport_trusting(ca, ScriptedPrompter::with_credentials("alice", "pw"), |c| {
        c.retry = RetryPolicy {
            delays: vec![SLOW_RETRY; 3],
        };
        tweak(c);
    })
}

fn req(m: &str, url: &str) -> HttpRequest {
    let mut r = HttpRequest::new(
        Method::from_bytes(m.as_bytes()).unwrap(),
        url.parse().unwrap(),
    );
    if !matches!(m, "GET" | "OPTIONS" | "DELETE" | "UNLOCK") {
        r.body = RequestBody::Bytes(bytes::Bytes::from_static(b"<x/>"));
    }
    r
}

async fn body_of(t: &HttpTransport, m: &str, url: &str) -> svn_types::Result<String> {
    let mut resp = t.send(req(m, url), &CancelToken::new()).await?;
    let mut out = Vec::new();
    while let Some(c) = resp.chunk().await? {
        out.extend_from_slice(&c);
    }
    Ok(String::from_utf8(out).unwrap())
}

#[tokio::test]
async fn a_close_without_close_notify_after_a_complete_body_is_not_an_error() {
    let ca = TestCa::new();
    let server = RawServer::start(
        &ca,
        vec![
            Act::OkThenUncleanClose("first".into()),
            Act::OkThenUncleanClose("second".into()),
        ],
    )
    .await;
    let t = transport(&ca, |_| {});
    assert_eq!(
        body_of(&t, "GET", &server.url("/a")).await.unwrap(),
        "first"
    );
    let read = t
        .send_and_read(
            req("PROPFIND", &server.url("/b")),
            1024,
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(read.body, "second");
    // The closed connection is not reused: the next request opens a new one and succeeds.
    assert_eq!(body_of(&t, "GET", &server.url("/c")).await.unwrap(), "ok");
    assert_eq!(server.seen().len(), 3);
}

#[tokio::test]
async fn a_reused_connection_closed_or_reset_under_a_read_is_retried_at_once() {
    for broken in [Act::CloseUnclean, Act::Reset] {
        let ca = TestCa::new();
        let server = RawServer::start(&ca, vec![Act::Ok("warm".into()), broken.clone()]).await;
        let t = transport(&ca, |_| {});
        assert_eq!(body_of(&t, "GET", &server.url("/a")).await.unwrap(), "warm");
        for m in ["GET", "PROPFIND", "REPORT", "OPTIONS"] {
            let started = Instant::now();
            assert_eq!(
                body_of(&t, m, &server.url("/b")).await.unwrap(),
                "ok",
                "{m} after {broken:?}"
            );
            // The first retry of a broken pooled connection does not wait for the backoff.
            assert!(
                started.elapsed() < SLOW_RETRY,
                "{m}: {:?}",
                started.elapsed()
            );
        }
        assert_eq!(server.seen()[..3], ["GET", "GET", "GET"], "{broken:?}");
        assert!(server.connections() >= 2);
    }
}

#[tokio::test]
async fn the_retry_after_a_broken_pooled_connection_does_not_reuse_another_stale_one() {
    let ca = TestCa::new();
    let server = RawServer::start_with(&ca, Vec::new(), true).await;
    let t = transport(&ca, |_| {});
    // Three pooled connections, each used once (held at the same time so none is reused).
    let cancel = CancelToken::new();
    let held = futures_util::future::join_all(
        (0..3).map(|i| t.send(req("GET", &server.url(&format!("/warm{i}"))), &cancel)),
    )
    .await;
    for resp in held {
        let mut resp = resp.unwrap();
        while resp.chunk().await.unwrap().is_some() {}
    }
    assert_eq!(server.connections(), 3);
    // Every idle connection is now closed by the server under its next request. The read
    // fails once on a pooled one and is retried at once on a new connection.
    let started = Instant::now();
    assert_eq!(
        body_of(&t, "PROPFIND", &server.url("/p")).await.unwrap(),
        "ok"
    );
    assert!(started.elapsed() < SLOW_RETRY, "{:?}", started.elapsed());
    assert_eq!(server.seen().len(), 5, "{:?}", server.seen());
    assert_eq!(server.connections(), 4);
}

#[tokio::test]
async fn writes_are_never_sent_twice() {
    // Every svn write method: commit (MKACTIVITY, POST txn, CHECKOUT, PUT, PROPPATCH, MKCOL,
    // COPY, DELETE, MERGE) and locks (LOCK, UNLOCK).
    let writes = [
        "MKACTIVITY",
        "POST",
        "CHECKOUT",
        "PUT",
        "PROPPATCH",
        "MKCOL",
        "COPY",
        "DELETE",
        "MERGE",
        "LOCK",
        "UNLOCK",
    ];
    for m in writes {
        assert!(!is_idempotent(&Method::from_bytes(m.as_bytes()).unwrap()));
        for (broken, failure) in [
            // Closed under the request without close_notify, as Apache on Windows does.
            (Act::CloseUnclean, ConnectionFailure::TlsClosedUncleanly),
            (Act::Reset, ConnectionFailure::ConnectionReset),
            (
                Act::Truncate {
                    body: "x".repeat(100),
                    keep: 10,
                },
                ConnectionFailure::TlsClosedUncleanly,
            ),
        ] {
            let ca = TestCa::new();
            let server = RawServer::start(&ca, vec![Act::Ok("warm".into()), broken.clone()]).await;
            let t = transport(&ca, |c| c.retry.delays = vec![Duration::ZERO; 3]);
            assert_eq!(body_of(&t, "GET", &server.url("/a")).await.unwrap(), "warm");
            let err = t
                .send_and_read(req(m, &server.url("/w")), 1024, &CancelToken::new())
                .await
                .expect_err("a broken write fails");
            assert_eq!(err.code, ErrorCode::Network, "{m} {broken:?}: {err:?}");
            assert_eq!(err.connection, Some(failure), "{m} {broken:?}: {err:?}");
            // The write reached the server once and was not repeated.
            assert_eq!(server.seen(), ["GET", m], "{m} {broken:?}");
        }
    }
}

#[tokio::test]
async fn a_body_cut_off_without_close_notify_is_a_typed_error() {
    let ca = TestCa::new();
    let server = RawServer::start(
        &ca,
        vec![
            Act::Truncate {
                body: "y".repeat(1000),
                keep: 100,
            },
            Act::TruncateChunked,
        ],
    )
    .await;
    let t = transport(&ca, |_| {});
    for path in ["/length", "/chunked"] {
        let err = body_of(&t, "GET", &server.url(path))
            .await
            .expect_err("truncated");
        assert_eq!(err.code, ErrorCode::Network, "{path}: {err:?}");
        assert_eq!(
            err.connection,
            Some(ConnectionFailure::TlsClosedUncleanly),
            "{path}: {err:?}"
        );
        assert!(err.message.contains("close_notify"), "{path}: {err:?}");
    }
    // A streamed body is never re-requested behind the caller's back.
    assert_eq!(server.seen().len(), 2);
}

#[tokio::test]
async fn send_and_read_fetches_a_cut_off_read_again() {
    let ca = TestCa::new();
    let server = RawServer::start(
        &ca,
        vec![
            Act::Truncate {
                body: "z".repeat(500),
                keep: 50,
            },
            Act::TruncateChunked,
            Act::Ok("complete".into()),
        ],
    )
    .await;
    let t = transport(&ca, |c| c.retry.delays = vec![Duration::from_millis(10); 3]);
    let read = t
        .send_and_read(req("REPORT", &server.url("/r")), 1024, &CancelToken::new())
        .await
        .expect("third attempt");
    assert_eq!(read.body, "complete");
    assert_eq!(server.seen(), ["REPORT", "REPORT", "REPORT"]);
}

#[tokio::test]
async fn send_and_read_gives_up_after_the_retry_budget() {
    let ca = TestCa::new();
    let cut = || Act::Truncate {
        body: "z".repeat(500),
        keep: 50,
    };
    let server = RawServer::start(&ca, vec![cut(), cut(), cut(), cut(), cut()]).await;
    let t = transport(&ca, |c| c.retry.delays = vec![Duration::from_millis(10); 3]);
    let err = t
        .send_and_read(req("GET", &server.url("/g")), 1024, &CancelToken::new())
        .await
        .expect_err("budget used up");
    assert_eq!(err.connection, Some(ConnectionFailure::TlsClosedUncleanly));
    // One attempt plus three retries.
    assert_eq!(server.seen().len(), 4);
}

#[tokio::test]
async fn a_stalled_body_is_a_typed_timeout() {
    let ca = TestCa::new();
    let server = RawServer::start(&ca, vec![Act::Stall]).await;
    let t = transport(&ca, |c| {
        c.read_timeout = Duration::from_millis(300);
        c.retry.delays = Vec::new();
    });
    let err = body_of(&t, "GET", &server.url("/s"))
        .await
        .expect_err("stalled");
    assert_eq!(err.code, ErrorCode::Timeout, "{err:?}");
    assert_eq!(err.connection, Some(ConnectionFailure::Timeout), "{err:?}");
}

#[tokio::test]
async fn idle_pooled_connections_are_dropped_before_the_server_closes_them() {
    let ca = TestCa::new();
    let server = RawServer::start(&ca, Vec::new()).await;
    let t = transport(&ca, |c| c.pool_idle_timeout = Duration::from_millis(200));
    assert_eq!(body_of(&t, "GET", &server.url("/1")).await.unwrap(), "ok");
    assert_eq!(body_of(&t, "GET", &server.url("/2")).await.unwrap(), "ok");
    assert_eq!(server.connections(), 1, "kept alive while in use");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(body_of(&t, "GET", &server.url("/3")).await.unwrap(), "ok");
    assert_eq!(
        server.connections(),
        2,
        "the idle connection was not reused"
    );
}

#[test]
fn the_default_idle_timeout_is_below_common_keep_alive_timeouts() {
    let config = svn_ra::transport::TransportConfig::default();
    // Apache's default KeepAliveTimeout is 5 s.
    assert!(config.pool_idle_timeout < Duration::from_secs(5));
    assert_eq!(
        config.pool_idle_timeout,
        svn_ra::transport::DEFAULT_POOL_IDLE_TIMEOUT
    );
}
