//! Hostile-server behaviour at the transport layer (see `fixtures/evil/README.md`): redirects to
//! other hosts or schemes, oversized and stalled bodies, certificates for another name, and the
//! hostile XML fixtures delivered over a real HTTPS connection and parsed by svn-protocol.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Reply, ScriptedPrompter, TestCa, TlsServer};
use reqwest::Method;
use svn_protocol::update::parse_update;
use svn_protocol::{Capabilities, Headers, ResponseHead, UrlBuilder};
use svn_ra::transport::{HttpRequest, RetryPolicy};
use svn_ra_api::CertDecision;
use svn_types::{CancelToken, ErrorCode, RepoUrl};

fn ok_server_handler() -> Arc<common::Handler> {
    Arc::new(|_| Reply::ok("stolen"))
}

/// Every redirect that leaves the origin, changes scheme or smuggles credentials is refused for
/// every read method, and the redirect target never sees a request.
#[tokio::test]
async fn redirects_off_origin_are_refused() {
    let ca = TestCa::new();
    let other = TlsServer::start(&ca.leaf(&["localhost"], false), ok_server_handler()).await;
    let op = other.addr.port();
    // The main server learns its own port only after binding; targets that need it are built
    // from a placeholder that the handler fills in.
    let targets: Vec<String> = vec![
        format!("https://localhost:{op}/steal"),
        "https://127.0.0.1:{self}/steal".into(),
        "http://localhost:{self}/steal".into(),
        format!("//127.0.0.1:{op}/steal"),
        format!("https:\\\\127.0.0.1:{op}\\steal"),
        "https://user:pw@localhost:{self}/steal".into(),
        format!("https://localhost:{{self}}@127.0.0.1:{op}/steal"),
        "file:///etc/passwd".into(),
        "javascript:alert(1)".into(),
        "https://evil.test.local/steal".into(),
    ];
    for target in targets {
        let t2 = target.clone();
        let port_cell: Arc<std::sync::OnceLock<u16>> = Arc::default();
        let pc = Arc::clone(&port_cell);
        let server = TlsServer::start(
            &ca.leaf(&["localhost"], false),
            Arc::new(move |seen: &common::Seen| {
                if seen.path == "/start" {
                    let me = pc.get().copied().unwrap_or(0);
                    Reply::status(302).header("location", t2.replace("{self}", &me.to_string()))
                } else {
                    Reply::ok("stolen")
                }
            }),
        )
        .await;
        port_cell.set(server.addr.port()).unwrap();
        let prompter = ScriptedPrompter::with_credentials("tester", "tester-pass");
        let t = common::transport_trusting(&ca, prompter, |c| c.retry = RetryPolicy::none());
        for m in ["GET", "PROPFIND", "REPORT", "OPTIONS"] {
            let req = HttpRequest::new(
                Method::from_bytes(m.as_bytes()).unwrap(),
                server.url("/start"),
            );
            let r = t.send(req, &CancelToken::new()).await;
            let err = r
                .err()
                .unwrap_or_else(|| panic!("{m} redirect to {target} was followed"));
            assert!(
                [
                    "different origin",
                    "credentials in the URL",
                    "only http and https",
                    "forbidden character"
                ]
                .iter()
                .any(|w| err.message.contains(w)),
                "{m} {target}: refused for the wrong reason: {}",
                err.message
            );
            assert!(
                matches!(
                    err.code,
                    ErrorCode::Network
                        | ErrorCode::ServerUnsupported
                        | ErrorCode::Internal
                        | ErrorCode::UnsafePath
                ),
                "{m} {target}: {err:?}"
            );
        }
        assert!(
            server.requests().iter().all(|s| s.path == "/start"),
            "{target}: redirect target on the same server was requested"
        );
    }
    assert!(
        other.requests().is_empty(),
        "the other origin was contacted"
    );
}

/// A same-origin `Location` the URL parser would rewrite (tab deleted, `\\` read as `/`) or
/// that carries an encoded control character or backslash is refused, not normalised and
/// followed.
#[tokio::test]
async fn redirects_with_parser_rewritten_locations_are_refused() {
    let ca = TestCa::new();
    for target in [
        "/start/.\t./steal",
        "/a\\..\\steal",
        "/a/%2e%09%2e/steal",
        "/a%5C..%5Csteal",
        "/a/%0D%0Asteal",
    ] {
        let server = TlsServer::start(
            &ca.leaf(&["localhost"], false),
            Arc::new(move |seen: &common::Seen| {
                if seen.path == "/start" {
                    Reply::status(302).header("location", target)
                } else {
                    Reply::ok("stolen")
                }
            }),
        )
        .await;
        let prompter = ScriptedPrompter::with_credentials("tester", "tester-pass");
        let t = common::transport_trusting(&ca, prompter, |c| c.retry = RetryPolicy::none());
        let req = HttpRequest::new(Method::GET, server.url("/start"));
        let err = t
            .send(req, &CancelToken::new())
            .await
            .err()
            .unwrap_or_else(|| panic!("redirect to {target:?} was followed"));
        assert_eq!(err.code, ErrorCode::UnsafePath, "{target:?}: {err:?}");
        assert!(
            server.requests().iter().all(|s| s.path == "/start"),
            "{target:?}: the rewritten target was requested"
        );
    }
}

/// A body far larger than the caller's limit fails fast with `Corrupt` instead of buffering.
#[tokio::test]
async fn oversized_body_is_cut_off_at_the_limit() {
    let ca = TestCa::new();
    let server = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|_| Reply::zeros(1 << 30)),
    )
    .await;
    let t = common::transport_trusting(&ca, Arc::new(ScriptedPrompter::default()), |_| {});
    let resp = t
        .send(
            HttpRequest::new(Method::GET, server.url("/huge")),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let err = resp.bytes(1 << 20).await.expect_err("over the limit");
    assert_eq!(err.code, ErrorCode::Corrupt);
}

/// A server that sends headers and then stalls is cut off by the read timeout.
#[tokio::test]
async fn stalled_server_times_out() {
    let ca = TestCa::new();
    let server = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(|_| {
            let mut r = Reply::ok("late");
            r.delay = Duration::from_secs(5);
            r
        }),
    )
    .await;
    let t = common::transport_trusting(&ca, Arc::new(ScriptedPrompter::default()), |c| {
        c.read_timeout = Duration::from_millis(300);
        c.retry = RetryPolicy::none();
    });
    let err = t
        .send(
            HttpRequest::new(Method::GET, server.url("/slow")),
            &CancelToken::new(),
        )
        .await
        .expect_err("stalled");
    assert!(
        matches!(err.code, ErrorCode::Timeout | ErrorCode::Network),
        "{err:?}"
    );
}

/// A certificate for another host name is a hard failure even when the user would accept
/// anything: no prompt, no override (CertDecision default-deny for hard failures).
#[tokio::test]
async fn certificate_for_another_host_is_never_accepted() {
    let ca = TestCa::new();
    for names in [
        &["evil.test.local"][..],
        &["*.test.local"][..],
        &["localhost.evil.test.local"][..],
    ] {
        let server = TlsServer::start(&ca.leaf(names, false), ok_server_handler()).await;
        for trust_ca in [true, false] {
            let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
            let t = common::transport_trusting(&ca, prompter.clone(), |c| {
                if !trust_ca {
                    c.extra_root_certs.clear();
                }
            });
            let err = t
                .send(
                    HttpRequest::new(Method::GET, server.url("/")),
                    &CancelToken::new(),
                )
                .await
                .expect_err("name mismatch");
            assert_eq!(err.code, ErrorCode::TlsUntrusted, "{names:?}: {err:?}");
            if trust_ca {
                assert_eq!(
                    prompter.cert_prompts(),
                    0,
                    "{names:?}: mismatch was promptable"
                );
            }
            assert!(
                server.requests().is_empty(),
                "{names:?}: request reached the server"
            );
        }
    }
}

fn evil_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/evil")
}

/// The hostile update reports, served over HTTPS and read through the transport, are rejected by
/// the parser exactly as they are offline.
#[tokio::test]
async fn hostile_update_reports_over_the_wire_are_rejected() {
    let index = std::fs::read_to_string(evil_dir().join("cases.tsv")).unwrap();
    let cases: Vec<(String, Vec<u8>)> = index
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('\t').collect();
            (cols[1] == "update").then(|| {
                (
                    cols[0].to_owned(),
                    std::fs::read(evil_dir().join(cols[2])).unwrap(),
                )
            })
        })
        .collect();
    assert!(cases.len() >= 20);
    let bodies = Arc::new(cases.clone());
    let b2 = Arc::clone(&bodies);
    let ca = TestCa::new();
    let server = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        Arc::new(move |seen: &common::Seen| {
            let i: usize = seen.path.trim_start_matches("/case/").parse().unwrap();
            let mut r = Reply::status(200).header("content-type", "text/xml");
            r.body = common::TestBody::Full(Some(bytes::Bytes::from(b2[i].1.clone())));
            r
        }),
    )
    .await;
    let t = common::transport_trusting(&ca, Arc::new(ScriptedPrompter::default()), |_| {});
    let session = RepoUrl::parse(server.url("/svn/repo").as_str()).unwrap();
    let mut h = Headers::new();
    for (k, v) in [
        ("SVN-Youngest-Rev", "5"),
        (
            "SVN-Repository-UUID",
            "7462d98e-4071-4508-b797-d2922b5cda8f",
        ),
        ("SVN-Repository-Root", "/svn/repo"),
        ("SVN-Me-Resource", "/svn/repo/!svn/me"),
        ("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr"),
        ("SVN-Rev-Stub", "/svn/repo/!svn/rev"),
        ("SVN-Txn-Root-Stub", "/svn/repo/!svn/txr"),
        ("SVN-Txn-Stub", "/svn/repo/!svn/txn"),
    ] {
        h.push(k, v);
    }
    let caps =
        Capabilities::from_options_response(&session, &ResponseHead::new(200, h), b"").unwrap();
    let urls = UrlBuilder::new(&session, &caps).unwrap();
    for (i, (id, _)) in cases.iter().enumerate() {
        let resp = t
            .send(
                HttpRequest::new(
                    Method::from_bytes(b"REPORT").unwrap(),
                    server.url(&format!("/case/{i}")),
                ),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let body = resp.bytes(1 << 20).await.unwrap();
        let err = parse_update(&urls, &body).expect_err(id);
        assert!(
            matches!(err.code, ErrorCode::UnsafePath | ErrorCode::Corrupt),
            "{id}: {err:?}"
        );
    }
}
