//! Plain HTTP behavior: methods, retry, timeouts, cancel, redirects, pool limit and body framing.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::{Reply, ScriptedPrompter, TestCa, TlsServer, transport_trusting};
use reqwest::Method;
use svn_ra::transport::{
    HttpRequest, HttpTransport, MemoryTrustStore, ProxyConfig, RequestBody, RetryPolicy,
    TransportConfig, classify_status, is_idempotent,
};
use svn_types::{CancelToken, ErrorCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        delays: vec![Duration::from_millis(10); 3],
    }
}

fn plain_transport(tweak: impl FnOnce(&mut TransportConfig)) -> HttpTransport {
    let mut config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        retry: fast_retry(),
        ..TransportConfig::default()
    };
    tweak(&mut config);
    HttpTransport::new(
        config,
        ScriptedPrompter::with_credentials("alice", "pw"),
        Arc::new(MemoryTrustStore::new()),
    )
}

fn req(m: &str, url: String) -> HttpRequest {
    HttpRequest::new(
        Method::from_bytes(m.as_bytes()).expect("method"),
        url.parse().expect("url"),
    )
}

async fn flaky(server: &MockServer, m: &str, failures: u64, failure_status: u16) {
    Mock::given(method(m))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(failure_status))
        .up_to_n_times(failures)
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method(m))
        .and(path("/x"))
        .respond_with(ResponseTemplate::new(200).set_body_string("done"))
        .with_priority(2)
        .mount(server)
        .await;
}

#[tokio::test]
async fn idempotent_methods_retry_transient_statuses_with_custom_methods() {
    for m in ["GET", "OPTIONS", "PROPFIND", "REPORT"] {
        let server = MockServer::start().await;
        flaky(&server, m, 2, 503).await;
        let t = plain_transport(|_| {});
        let resp = t
            .send(req(m, format!("{}/x", server.uri())), &CancelToken::new())
            .await
            .expect(m);
        assert_eq!(resp.status().as_u16(), 200, "{m}");
        assert_eq!(server.received_requests().await.unwrap().len(), 3, "{m}");
    }
}

#[tokio::test]
async fn retries_are_limited_to_three_and_the_last_response_is_returned() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let t = plain_transport(|_| {});
    let resp = t
        .send(
            req("GET", format!("{}/x", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect("response");
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
    assert_eq!(classify_status(503), Some(ErrorCode::Network));
}

#[tokio::test]
async fn default_retry_policy_backs_off_half_one_two_seconds() {
    let d = RetryPolicy::default().delays;
    assert_eq!(
        d,
        vec![
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2)
        ]
    );
}

#[tokio::test]
async fn non_idempotent_methods_are_sent_once() {
    for m in [
        "POST", "PUT", "MERGE", "MKCOL", "LOCK", "CHECKOUT", "DELETE",
    ] {
        assert!(
            !is_idempotent(&Method::from_bytes(m.as_bytes()).unwrap()),
            "{m}"
        );
        let server = MockServer::start().await;
        flaky(&server, m, 5, 503).await;
        let t = plain_transport(|_| {});
        let resp = t
            .send(req(m, format!("{}/x", server.uri())), &CancelToken::new())
            .await
            .expect(m);
        assert_eq!(resp.status().as_u16(), 503, "{m}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1, "{m}");
    }
}

/// Raw server: drops the first `drops` connections without answering, then serves 200.
async fn reset_then_ok(drops: usize) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen2 = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let n = seen2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                if n < drops {
                    drop(sock);
                    return;
                }
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    (port, seen)
}

#[tokio::test]
async fn connection_resets_are_retried_for_idempotent_requests_only() {
    let (port, seen) = reset_then_ok(2).await;
    let t = plain_transport(|_| {});
    let resp = t
        .send(
            req("PROPFIND", format!("http://127.0.0.1:{port}/")),
            &CancelToken::new(),
        )
        .await
        .expect("recovered");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 3);

    let (port, seen) = reset_then_ok(1).await;
    let err = t
        .send(
            req("MERGE", format!("http://127.0.0.1:{port}/")),
            &CancelToken::new(),
        )
        .await
        .expect_err("not retried");
    assert_eq!(err.code, ErrorCode::Network);
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn read_timeout_maps_to_timeout_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let t = plain_transport(|c| {
        c.read_timeout = Duration::from_millis(200);
        c.retry = RetryPolicy::none();
    });
    let start = Instant::now();
    let err = t
        .send(
            req("GET", format!("{}/", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect_err("timeout");
    assert_eq!(err.code, ErrorCode::Timeout);
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn cancel_interrupts_a_pending_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .mount(&server)
        .await;
    let t = plain_transport(|_| {});
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let start = Instant::now();
    let err = t
        .send(req("GET", format!("{}/", server.uri())), &cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn cancel_during_retry_backoff_stops_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let t = plain_transport(|c| {
        c.retry = RetryPolicy {
            delays: vec![Duration::from_secs(30)],
        }
    });
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c2.cancel();
    });
    let err = t
        .send(req("GET", format!("{}/", server.uri())), &cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn cancel_in_the_middle_of_a_download_stops_the_stream() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|_| Reply::zeros(10 * 1024 * 1024 * 1024)),
    )
    .await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |_| {});
    let cancel = CancelToken::new();
    let mut resp = t
        .send(HttpRequest::new(Method::GET, srv.url("/big")), &cancel)
        .await
        .expect("headers");
    let first = resp.chunk().await.expect("chunk").expect("data");
    assert!(!first.is_empty());
    cancel.cancel();
    let err = resp.chunk().await.expect_err("cancelled mid-stream");
    assert_eq!(err.code, ErrorCode::Cancelled);
}

#[tokio::test]
async fn basic_over_plain_http_is_refused_before_any_prompt() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).insert_header("www-authenticate", "Basic realm=\"r\""),
        )
        .mount(&server)
        .await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let t = HttpTransport::new(
        TransportConfig {
            proxy: ProxyConfig::Disabled,
            ..TransportConfig::default()
        },
        prompter.clone(),
        Arc::new(MemoryTrustStore::new()),
    );
    let err = t
        .send(
            req("GET", format!("{}/", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect_err("refused");
    assert_eq!(err.code, ErrorCode::AuthRequired);
    assert!(err.message.contains("plain http"), "{}", err.message);
    assert_eq!(prompter.credential_prompts(), 0);
    let received = server.received_requests().await.unwrap();
    assert!(
        received
            .iter()
            .all(|r| !r.headers.contains_key("authorization"))
    );
}

#[tokio::test]
async fn same_origin_redirect_is_followed_and_cross_origin_is_blocked() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(200).set_body_string("moved"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/away"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/steal", other.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/old"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/new"))
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(207))
        .mount(&server)
        .await;

    let t = plain_transport(|_| {});
    let resp = t
        .send(
            req("GET", format!("{}/old", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect("followed");
    assert!(resp.url().path().ends_with("/new"));
    assert_eq!(resp.bytes(100).await.unwrap(), Bytes::from_static(b"moved"));

    let resp = t
        .send(
            req("PROPFIND", format!("{}/old", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect("method preserved on 307");
    assert_eq!(resp.status().as_u16(), 207);

    let err = t
        .send(
            req("GET", format!("{}/away", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect_err("cross origin");
    assert_eq!(err.code, ErrorCode::Network);
    assert!(err.message.contains("different origin"), "{}", err.message);
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirect_loops_are_cut_off() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    let t = plain_transport(|_| {});
    let err = t
        .send(
            req("GET", format!("{}/loop", server.uri())),
            &CancelToken::new(),
        )
        .await
        .expect_err("loop");
    assert!(err.message.contains("too many redirects"));
}

#[tokio::test]
async fn bodies_use_content_length_when_known_and_chunked_otherwise() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|_| Reply::status(201)),
    )
    .await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |_| {});
    let mk_stream = || {
        Box::pin(futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ]))
    };
    let mut r1 = HttpRequest::new(Method::PUT, srv.url("/known"));
    r1.body = RequestBody::Stream {
        stream: Some(mk_stream()),
        len: Some(6),
    };
    let mut r2 = HttpRequest::new(Method::PUT, srv.url("/unknown"));
    r2.body = RequestBody::Stream {
        stream: Some(mk_stream()),
        len: None,
    };
    let mut r3 = HttpRequest::new(Method::from_bytes(b"MKCOL").unwrap(), srv.url("/bytes"));
    r3.body = RequestBody::Bytes(Bytes::from_static(b"<x/>"));
    for r in [r1, r2, r3] {
        t.send(r, &CancelToken::new()).await.expect("sent");
    }
    let seen = srv.requests();
    assert_eq!(seen[0].header("content-length").as_deref(), Some("6"));
    assert_eq!(seen[0].header("transfer-encoding"), None);
    assert_eq!(
        seen[1].header("transfer-encoding").as_deref(),
        Some("chunked")
    );
    assert_eq!(seen[1].body_len, 6);
    assert_eq!(seen[2].method.as_str(), "MKCOL");
    assert_eq!(seen[2].header("content-length").as_deref(), Some("4"));
}

#[tokio::test]
async fn connections_per_host_are_bounded_and_reused() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|_| {
            let mut r = Reply::ok("x");
            r.delay = Duration::from_millis(100);
            r
        }),
    )
    .await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |c| {
        c.max_connections_per_host = 2;
    });
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let t = Arc::clone(&t);
            let url = srv.url(&format!("/{i}"));
            tokio::spawn(async move {
                let r = t
                    .send(HttpRequest::new(Method::GET, url), &CancelToken::new())
                    .await?;
                r.bytes(10).await
            })
        })
        .collect();
    for h in handles {
        h.await.unwrap().expect("ok");
    }
    let conns = srv.connections.load(std::sync::atomic::Ordering::SeqCst);
    assert!(conns <= 2, "opened {conns} connections");
    assert_eq!(srv.requests().len(), 8);
}

#[tokio::test]
async fn connection_limit_is_clamped_to_serf_range() {
    let c = |n| TransportConfig {
        max_connections_per_host: n,
        ..TransportConfig::default()
    };
    assert_eq!(c(0).effective_connections(), 1);
    assert_eq!(c(4).effective_connections(), 4);
    assert_eq!(c(100).effective_connections(), 8);
    assert_eq!(TransportConfig::default().effective_connections(), 4);
}

#[tokio::test]
async fn write_methods_are_never_redirected() {
    let server = MockServer::start().await;
    Mock::given(path("/old"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/new"))
        .mount(&server)
        .await;
    let t = plain_transport(|_| {});
    for m in [
        "PUT",
        "MKCOL",
        "DELETE",
        "COPY",
        "MOVE",
        "PROPPATCH",
        "MERGE",
        "POST",
        "LOCK",
        "UNLOCK",
    ] {
        let err = t
            .send(req(m, format!("{}/old", server.uri())), &CancelToken::new())
            .await
            .expect_err(m);
        assert!(
            err.message.contains("repository moved"),
            "{m}: {}",
            err.message
        );
        assert!(err.message.contains("/new"), "{m}: {}", err.message);
    }
    let hits = server.received_requests().await.unwrap();
    assert!(hits.iter().all(|r| r.url.path() == "/old"));
    assert_eq!(hits.len(), 10);
}

#[tokio::test]
async fn a_held_streaming_response_does_not_starve_parallel_requests() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|seen| {
            if seen.method.as_str() == "REPORT" {
                Reply::zeros(10 * 1024 * 1024 * 1024)
            } else {
                Reply::ok("x")
            }
        }),
    )
    .await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |c| {
        c.max_connections_per_host = 2;
    });
    let mut report = t
        .send(
            HttpRequest::new(Method::from_bytes(b"REPORT").unwrap(), srv.url("/r")),
            &CancelToken::new(),
        )
        .await
        .expect("report");
    assert!(report.chunk().await.unwrap().is_some());
    let gets: Vec<_> = (0..8)
        .map(|i| {
            let t = Arc::clone(&t);
            let url = srv.url(&format!("/g{i}"));
            tokio::spawn(async move {
                let r = t
                    .send(HttpRequest::new(Method::GET, url), &CancelToken::new())
                    .await?;
                r.bytes(10).await
            })
        })
        .collect();
    for g in gets {
        tokio::time::timeout(Duration::from_secs(10), g)
            .await
            .expect("no deadlock")
            .unwrap()
            .expect("get");
    }
    assert!(report.chunk().await.unwrap().is_some());
}

#[tokio::test]
async fn the_connection_slot_is_released_when_the_body_ends() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|_| Reply::ok("body")),
    )
    .await;
    let t = transport_trusting(&ca, ScriptedPrompter::with_credentials("a", "b"), |c| {
        c.max_connections_per_host = 1;
    });
    let mut first = t
        .send(
            HttpRequest::new(Method::GET, srv.url("/1")),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    while first.chunk().await.unwrap().is_some() {}
    // `first` is still alive, but its body is finished, so the only slot is free again.
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        t.send(
            HttpRequest::new(Method::GET, srv.url("/2")),
            &CancelToken::new(),
        ),
    )
    .await
    .expect("slot released at end of body")
    .expect("second");
    assert_eq!(second.status().as_u16(), 200);
    drop(first);
}
