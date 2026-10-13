//! Secrets must never reach logs, at any level.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::io;
use std::sync::{Arc, Mutex};

use common::{
    Reply, ScriptedPrompter, TestCa, TlsServer, basic, basic_auth_handler, transport_trusting,
};
use proptest::prelude::*;
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use svn_ra::transport::{
    HttpRequest, RedactedHeaders, RedactingMakeWriter, redact_text, redacted_url,
};
use svn_types::CancelToken;
use tracing_subscriber::fmt::MakeWriter;

/// In-memory log sink.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

impl Sink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn capture() -> (Sink, tracing::subscriber::DefaultGuard) {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(RedactingMakeWriter::new(sink.clone()))
        .finish();
    (sink, tracing::subscriber::set_default(subscriber))
}

#[test]
fn redacts_known_shapes() {
    let cases = [
        ("Authorization: Basic dXNlcjpwdw==", "dXNlcjpwdw"),
        ("proxy-authorization=Bearer abc123", "abc123"),
        (
            r#"headers={"cookie": "sid=topsecret; a=b", "x": "1"}"#,
            "topsecret",
        ),
        ("Set-Cookie: session=hunter22; Path=/", "hunter22"),
        ("Lock-Token: <opaquelocktoken:1234-abcd>", "1234-abcd"),
        ("If: (<opaquelocktoken:feed-beef>)", "feed-beef"),
        (
            "GET https://alice:p%40ss@svn.test.local/repo failed",
            "p%40ss",
        ),
        ("GET https://alice@svn.test.local/repo", "alice"),
    ];
    for (input, secret) in cases {
        let out = redact_text(input);
        assert!(!out.contains(secret), "{input} -> {out}");
    }
    // Non-secret text is untouched.
    assert_eq!(redact_text("diff verified: ok"), "diff verified: ok");
    assert_eq!(
        redact_text("GET https://svn.test.local/repo"),
        "GET https://svn.test.local/repo"
    );
}

#[test]
fn redacted_headers_and_urls_hide_values() {
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_static("Basic c2VjcmV0"));
    h.insert(
        "lock-token",
        HeaderValue::from_static("<opaquelocktoken:zzz>"),
    );
    h.insert("depth", HeaderValue::from_static("1"));
    let shown = RedactedHeaders(&h).to_string();
    assert!(
        !shown.contains("c2VjcmV0") && !shown.contains("zzz"),
        "{shown}"
    );
    assert!(shown.contains("depth: 1"));
    let u = url::Url::parse("https://alice:pw@svn.test.local:8443/r").unwrap();
    assert_eq!(redacted_url(&u), "https://svn.test.local:8443/r");
}

proptest! {
    /// Random secrets placed in every sensitive position never survive in log output.
    #[test]
    fn random_secrets_never_appear_in_logs(
        password in "zq[A-Za-z0-9!#$%&*+=?^_~.-]{14,30}",
        token in "zq[A-Za-z0-9+/=_-]{14,30}",
        cookie in "zq[A-Za-z0-9._~-]{14,30}",
        lock in "zq[a-f0-9-]{14,30}",
        user in "[a-z]{4,8}",
    ) {
        let (sink, _guard) = capture();
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", format!("Basic {token}")),
            ("proxy-authorization", format!("Basic {token}")),
            ("cookie", format!("sid={cookie}")),
            ("set-cookie", format!("sid={cookie}; HttpOnly")),
            ("lock-token", format!("<opaquelocktoken:{lock}>")),
            ("if", format!("(<opaquelocktoken:{lock}>)")),
        ] {
            headers.insert(HeaderName::from_static(name), HeaderValue::from_str(&value).unwrap());
        }
        let mut url = url::Url::parse("https://svn.test.local/repo").unwrap();
        url.set_username(&user).unwrap();
        url.set_password(Some(&password)).unwrap();

        // Through the transport's own helpers.
        tracing::info!(headers = %RedactedHeaders(&headers), url = %redacted_url(&url), "request");
        // Through careless logging that bypasses the helpers: the writer layer must catch it.
        tracing::debug!(?headers, "raw headers");
        tracing::trace!(%url, "raw url");
        tracing::warn!("Authorization: Basic {token}");
        tracing::error!("failed GET {url} with Cookie: sid={cookie}");
        tracing::info!(authorization = %format!("Basic {token}"), "field");

        let out = sink.text();
        let encoded_pw = url.password().unwrap().to_owned();
        for secret in [&password, &encoded_pw, &token, &cookie, &lock] {
            prop_assert!(!out.contains(secret.as_str()), "leaked {secret} in:\n{out}");
        }
        prop_assert!(!out.contains(&format!("{user}:")), "userinfo leaked in:\n{out}");
    }
}

#[tokio::test]
async fn a_full_authenticated_exchange_leaves_no_secret_in_trace_logs() {
    let ca = TestCa::new();
    let srv = TlsServer::start(
        &ca.leaf(&["localhost"], false),
        basic_auth_handler("r", "alice", "Tr0ub4dor&3-unique-pw"),
    )
    .await;
    let prompter = ScriptedPrompter::with_credentials("alice", "Tr0ub4dor&3-unique-pw");
    let t = transport_trusting(&ca, prompter, |_| {});
    let (sink, _guard) = capture();
    let mut req = HttpRequest::new(Method::from_bytes(b"LOCK").unwrap(), srv.url("/f"));
    req.headers.insert(
        "lock-token",
        HeaderValue::from_static("<opaquelocktoken:unique-lock-123>"),
    );
    req.headers
        .insert("cookie", HeaderValue::from_static("sid=unique-cookie-456"));
    let resp = t.send(req, &CancelToken::new()).await.expect("ok");
    assert_eq!(resp.status().as_u16(), 200);

    let out = sink.text();
    assert!(out.contains("http request"), "transport should log: {out}");
    let b64 = basic("alice", "Tr0ub4dor&3-unique-pw");
    for secret in [
        "Tr0ub4dor",
        "unique-lock-123",
        "unique-cookie-456",
        b64.trim_start_matches("Basic "),
    ] {
        assert!(!out.contains(secret), "leaked {secret}:\n{out}");
    }
    // Keep the unused-import check honest: the server really saw the credentials.
    assert_eq!(
        srv.requests().last().unwrap().header("authorization"),
        Some(b64)
    );
    let _ = Reply::status(200);
}
