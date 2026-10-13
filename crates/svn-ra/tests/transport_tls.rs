//! TLS policy: which certificate failures can be overridden, pinning and changed certificates.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::{Reply, ScriptedPrompter, TestCa, TlsServer};
use reqwest::Method;
use svn_ra::transport::{
    HttpRequest, HttpTransport, MemoryTrustStore, ProxyConfig, TransportConfig, TrustStore,
    sha256_fingerprint,
};
use svn_ra_api::{CertDecision, CertFailure};
use svn_types::{CancelToken, ErrorCode};

fn plain_transport(
    prompter: Arc<ScriptedPrompter>,
    trust: Arc<MemoryTrustStore>,
    roots: Vec<rustls::pki_types::CertificateDer<'static>>,
) -> HttpTransport {
    let config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        extra_root_certs: roots,
        ..TransportConfig::default()
    };
    HttpTransport::new(config, prompter, trust)
}

async fn get(t: &HttpTransport, url: url::Url) -> svn_types::Result<u16> {
    let resp = t
        .send(HttpRequest::new(Method::GET, url), &CancelToken::new())
        .await?;
    Ok(resp.status().as_u16())
}

fn ok_handler() -> Arc<common::Handler> {
    Arc::new(|_| Reply::ok("hello"))
}

#[tokio::test]
async fn certificate_from_trusted_root_needs_no_prompt() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["localhost"], false), ok_handler()).await;
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::Reject);
    let t = plain_transport(prompter.clone(), Arc::default(), vec![ca.der()]);
    assert_eq!(get(&t, server.url("/")).await.expect("ok"), 200);
    assert_eq!(prompter.cert_prompts(), 0);
}

#[tokio::test]
async fn unknown_issuer_is_reported_and_reject_blocks() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["localhost"], false), ok_handler()).await;
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::Reject);
    let t = plain_transport(prompter.clone(), Arc::default(), vec![]);
    let err = get(&t, server.url("/")).await.expect_err("untrusted");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    let calls = prompter.cert_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].failure, CertFailure::UnknownIssuer);
    assert_eq!(calls[0].host, "localhost");
    assert_eq!(calls[0].port, server.addr.port());
    assert_eq!(calls[0].sha256, sha256_fingerprint(&server.leaf_der));
    assert!(!calls[0].issuer.is_empty());
    assert!(calls[0].not_before.0 < calls[0].not_after.0);
}

#[tokio::test]
async fn accept_once_trusts_for_the_session_only() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["localhost"], false), ok_handler()).await;
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptOnce);
    let trust = Arc::new(MemoryTrustStore::new());
    let t = plain_transport(prompter.clone(), trust.clone(), vec![]);
    assert_eq!(get(&t, server.url("/a")).await.expect("ok"), 200);
    assert_eq!(get(&t, server.url("/b")).await.expect("ok"), 200);
    assert_eq!(prompter.cert_prompts(), 1);
    assert!(trust.pinned("localhost", server.addr.port()).is_empty());
    // A new transport (new session) asks again.
    let t2 = plain_transport(prompter.clone(), trust, vec![]);
    assert_eq!(get(&t2, server.url("/c")).await.expect("ok"), 200);
    assert_eq!(prompter.cert_prompts(), 2);
}

#[tokio::test]
async fn self_signed_accept_always_pins_host_port_and_fingerprint() {
    let leaf = TestCa::self_signed_leaf(&["localhost"]);
    let server = TlsServer::start(&leaf, ok_handler()).await;
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
    let trust = Arc::new(MemoryTrustStore::new());
    let t = plain_transport(prompter.clone(), trust.clone(), vec![]);
    assert_eq!(get(&t, server.url("/")).await.expect("ok"), 200);
    let calls = prompter.cert_calls.lock().unwrap().clone();
    assert_eq!(calls[0].failure, CertFailure::SelfSigned);
    let fp = sha256_fingerprint(&server.leaf_der);
    assert_eq!(trust.pinned("localhost", server.addr.port()), vec![fp]);
    // Different port: not pinned.
    assert!(trust.pinned("localhost", server.addr.port() + 1).is_empty());
    // New session with the pin: no prompt.
    let quiet = ScriptedPrompter::with_cert_decision(CertDecision::Reject);
    let t2 = plain_transport(quiet.clone(), trust, vec![]);
    assert_eq!(get(&t2, server.url("/")).await.expect("ok"), 200);
    assert_eq!(quiet.cert_prompts(), 0);
}

#[tokio::test]
async fn hostname_mismatch_is_blocked_without_prompting() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["other.test.local"], false), ok_handler()).await;
    // Connect by a name that the certificate does not cover.
    let url = url::Url::parse(&format!("https://localhost:{}/", server.addr.port())).unwrap();
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
    let t = plain_transport(prompter.clone(), Arc::default(), vec![ca.der()]);
    let err = get(&t, url).await.expect_err("mismatch");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert!(err.message.contains("host name"), "{}", err.message);
    assert_eq!(prompter.cert_prompts(), 0);
}

#[tokio::test]
async fn expired_certificate_is_blocked_without_prompting() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["localhost"], true), ok_handler()).await;
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
    let t = plain_transport(prompter.clone(), Arc::default(), vec![ca.der()]);
    let err = get(&t, server.url("/")).await.expect_err("expired");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert!(err.message.contains("expired"), "{}", err.message);
    assert_eq!(prompter.cert_prompts(), 0);
}

#[tokio::test]
async fn pin_does_not_override_hard_failures() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["localhost"], true), ok_handler()).await;
    let trust = Arc::new(MemoryTrustStore::new());
    trust.pin(
        "localhost",
        server.addr.port(),
        sha256_fingerprint(&server.leaf_der),
    );
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
    let t = plain_transport(prompter.clone(), trust, vec![ca.der()]);
    let err = get(&t, server.url("/"))
        .await
        .expect_err("expired despite pin");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
}

#[tokio::test]
async fn changed_pinned_certificate_is_reported_distinctly_and_replaces_the_pin_only_on_accept() {
    let first = TestCa::self_signed_leaf(&["localhost"]);
    let replacement = TestCa::self_signed_leaf(&["localhost"]);
    let server = TlsServer::start(&replacement, ok_handler()).await;
    let trust = Arc::new(MemoryTrustStore::new());
    let old_fp = sha256_fingerprint(&first.cert_der);
    trust.pin("localhost", server.addr.port(), old_fp);

    let rejecting = ScriptedPrompter::with_cert_decision(CertDecision::Reject);
    let t = plain_transport(rejecting.clone(), trust.clone(), vec![]);
    let err = get(&t, server.url("/")).await.expect_err("changed");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert!(err.message.contains("changed"), "{}", err.message);
    let calls = rejecting.cert_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(
        matches!(
            calls[0].failure,
            CertFailure::Changed { old_sha256, new_sha256 }
                if old_sha256 == old_fp && new_sha256 == sha256_fingerprint(&replacement.cert_der)
        ),
        "{:?}",
        calls[0].failure
    );

    // AcceptOnce and AcceptAlways are invalid for a changed certificate: treated as a rejection,
    // and the old pin stays.
    for invalid in [CertDecision::AcceptOnce, CertDecision::AcceptAlways] {
        let p = ScriptedPrompter::with_cert_decision(invalid);
        let t = plain_transport(p.clone(), trust.clone(), vec![]);
        let err = get(&t, server.url("/"))
            .await
            .expect_err("invalid decision");
        assert_eq!(err.code, ErrorCode::TlsUntrusted, "{invalid:?}");
        assert_eq!(p.cert_prompts(), 1);
        assert_eq!(trust.pinned("localhost", server.addr.port()), vec![old_fp]);
    }

    // `ReplacePin` replaces the old pin, after the explicit prompt.
    let accepting = ScriptedPrompter::with_cert_decision(CertDecision::ReplacePin);
    let t2 = plain_transport(accepting.clone(), trust.clone(), vec![]);
    assert_eq!(get(&t2, server.url("/")).await.expect("accepted"), 200);
    assert_eq!(accepting.cert_prompts(), 1);
    let new_fp = sha256_fingerprint(&replacement.cert_der);
    assert_eq!(trust.pinned("localhost", server.addr.port()), vec![new_fp]);
    // The new pin is honored without another prompt.
    let t3 = plain_transport(accepting.clone(), trust, vec![]);
    assert_eq!(get(&t3, server.url("/")).await.expect("pinned"), 200);
    assert_eq!(accepting.cert_prompts(), 1);
}

#[tokio::test]
async fn untrusted_certificate_for_the_wrong_host_is_not_overridable() {
    // Self-signed and issued for another name: the user must not be able to pin it.
    let leaf = TestCa::self_signed_leaf(&["other.test.local"]);
    let server = TlsServer::start(&leaf, ok_handler()).await;
    let url = url::Url::parse(&format!("https://localhost:{}/", server.addr.port())).unwrap();
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
    let t = plain_transport(prompter.clone(), Arc::default(), vec![]);
    let err = get(&t, url).await.expect_err("mismatch");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert_eq!(prompter.cert_prompts(), 0);
}

#[tokio::test]
async fn changed_and_hostname_mismatched_is_reported_as_hostname_mismatch() {
    // Pinned certificate differs from the presented one (changed) and the presented one is for
    // another host: the hard failure wins, nothing is offered to the user.
    let first = TestCa::self_signed_leaf(&["localhost"]);
    let presented = TestCa::self_signed_leaf(&["other.test.local"]);
    let server = TlsServer::start(&presented, ok_handler()).await;
    let trust = Arc::new(MemoryTrustStore::new());
    trust.pin(
        "localhost",
        server.addr.port(),
        sha256_fingerprint(&first.cert_der),
    );
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptAlways);
    let t = plain_transport(prompter.clone(), trust.clone(), vec![]);
    let err = get(&t, server.url("/")).await.expect_err("blocked");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert!(err.message.contains("host name"), "{}", err.message);
    assert!(!err.message.contains("changed"), "{}", err.message);
    assert_eq!(prompter.cert_prompts(), 0);
    assert_eq!(
        trust.pinned("localhost", server.addr.port()),
        vec![sha256_fingerprint(&first.cert_der)]
    );
}

#[tokio::test]
async fn unattended_send_never_asks_about_a_certificate() {
    let ca = TestCa::new();
    let server = TlsServer::start(&ca.leaf(&["localhost"], false), ok_handler()).await;
    let prompter = ScriptedPrompter::with_cert_decision(CertDecision::AcceptOnce);
    let t = plain_transport(prompter.clone(), Arc::default(), vec![]);
    let err = t
        .send_unattended(
            HttpRequest::new(Method::GET, server.url("/a")),
            &CancelToken::new(),
        )
        .await
        .expect_err("needs a decision");
    assert_eq!(err.code, ErrorCode::TlsUntrusted);
    assert_eq!(prompter.cert_prompts(), 0);
    // A certificate the user already accepted for the session is used silently.
    assert_eq!(get(&t, server.url("/b")).await.expect("ok"), 200);
    let resp = t
        .send_unattended(
            HttpRequest::new(Method::GET, server.url("/c")),
            &CancelToken::new(),
        )
        .await
        .expect("accepted earlier");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(prompter.cert_prompts(), 1);
}
