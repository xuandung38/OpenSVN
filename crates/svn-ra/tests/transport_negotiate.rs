//! Negotiate (SPNEGO/Kerberos) with a scripted provider: SSO, mutual authentication, fallback to a
//! Basic prompt with `NegotiateFailed`, blocking calls kept off the runtime thread, cancellation,
//! https-only, fresh tokens on retries, failure expiry, IP hosts and channel bindings.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{
    Handler, Reply, ScriptedPrompter, TestCa, TlsServer, basic, transport_with_negotiate,
};
use reqwest::Method;
use svn_ra::auth::{NegotiateContext, NegotiateProvider, NegotiateStep};
use svn_ra::transport::HttpRequest;
use svn_ra_api::PromptReason;
use svn_types::{CancelToken, ErrorCode, Result, SvnError};

const CLIENT_TOKEN: &[u8] = b"client-ap-req";
const SERVER_FINAL: &[u8] = b"server-ap-rep";

fn b64(t: &[u8]) -> String {
    STANDARD.encode(t)
}

/// Provider whose contexts accept exactly `SERVER_FINAL` as the mutual-authentication token.
#[derive(Default)]
struct FakeProvider {
    /// `initiate` fails with this message (no ticket, unknown SPN, ...).
    fail_initiate: Mutex<Option<String>>,
    /// `initiate` blocks the calling thread this long (KDC round trip).
    block_for: Mutex<Duration>,
    initiated: AtomicUsize,
    stepped: Arc<Mutex<Vec<Vec<u8>>>>,
    hosts: Mutex<Vec<String>>,
    threads: Arc<Mutex<Vec<ThreadId>>>,
    /// Channel bindings passed to each `initiate`.
    bindings: Mutex<Vec<Option<Vec<u8>>>>,
    /// Number each token (`client-ap-req#1`, `#2`, ...) instead of reusing `CLIENT_TOKEN`.
    unique_tokens: bool,
}

struct FakeContext {
    stepped: Arc<Mutex<Vec<Vec<u8>>>>,
    threads: Arc<Mutex<Vec<ThreadId>>>,
}

impl NegotiateProvider for FakeProvider {
    fn initiate(
        &self,
        host: &str,
        channel_bindings: Option<&[u8]>,
    ) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)> {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.hosts.lock().unwrap().push(host.to_owned());
        self.bindings
            .lock()
            .unwrap()
            .push(channel_bindings.map(<[u8]>::to_vec));
        std::thread::sleep(*self.block_for.lock().unwrap());
        let n = self.initiated.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(msg) = self.fail_initiate.lock().unwrap().clone() {
            return Err(SvnError::new(ErrorCode::NegotiateFailed, msg));
        }
        let token = if self.unique_tokens {
            format!("client-ap-req#{n}").into_bytes()
        } else {
            CLIENT_TOKEN.to_vec()
        };
        Ok((
            Box::new(FakeContext {
                stepped: Arc::clone(&self.stepped),
                threads: Arc::clone(&self.threads),
            }),
            token,
        ))
    }
}

impl NegotiateContext for FakeContext {
    fn step(&mut self, token: &[u8]) -> Result<NegotiateStep> {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        self.stepped.lock().unwrap().push(token.to_vec());
        if token == SERVER_FINAL {
            Ok(NegotiateStep::Complete)
        } else {
            Err(SvnError::new(ErrorCode::NegotiateFailed, "bad AP-REP"))
        }
    }
}

/// Server for `Negotiate` (+ optional Basic): accepts the fake client token and answers with
/// `final_token` (if any), accepts Basic `alice:pw`.
fn negotiate_handler(
    offer_basic: bool,
    accept_ticket: bool,
    final_token: Option<&'static [u8]>,
) -> Arc<Handler> {
    Arc::new(move |seen| {
        let auth = seen.header("authorization").unwrap_or_default();
        if accept_ticket && auth == format!("Negotiate {}", b64(CLIENT_TOKEN)) {
            let mut r = Reply::ok("sso");
            if let Some(t) = final_token {
                r = r.header("www-authenticate", format!("Negotiate {}", b64(t)));
            }
            return r;
        }
        if auth == basic("alice", "pw") {
            return Reply::ok("basic");
        }
        let mut r = Reply::status(401).header("www-authenticate", "Negotiate");
        if offer_basic {
            r = r.header("www-authenticate", "Basic realm=\"VisualSVN Server\"");
        }
        r
    })
}

async fn server(handler: Arc<Handler>) -> (TestCa, TlsServer) {
    let ca = TestCa::new();
    let s = TlsServer::start(&ca.leaf(&["localhost"], false), handler).await;
    (ca, s)
}

fn get(url: url::Url) -> HttpRequest {
    HttpRequest::new(Method::GET, url)
}

#[tokio::test]
async fn sso_succeeds_without_prompt_and_verifies_mutual_token() {
    let (ca, srv) = server(negotiate_handler(true, true, Some(SERVER_FINAL))).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());

    let r = t
        .send(get(srv.url("/svn/repo")), &CancelToken::new())
        .await
        .expect("sso");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(provider.hosts.lock().unwrap().as_slice(), ["localhost"]);
    assert_eq!(
        provider.stepped.lock().unwrap().as_slice(),
        [SERVER_FINAL.to_vec()]
    );
    // Both blocking calls ran on spawn_blocking threads, not on the (current-thread) runtime.
    let me = std::thread::current().id();
    let threads = provider.threads.lock().unwrap().clone();
    assert_eq!(threads.len(), 2);
    assert!(
        threads.iter().all(|t| *t != me),
        "blocking call on runtime thread"
    );

    let seen = srv.requests();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].header("authorization").is_none());
    assert_eq!(
        seen[1].header("authorization"),
        Some(format!("Negotiate {}", b64(CLIENT_TOKEN)))
    );

    // RFC 5929 tls-server-end-point binding of the server certificate (ECDSA P-256/SHA-256).
    let mut expected = b"tls-server-end-point:".to_vec();
    expected.extend_from_slice(&svn_ra::transport::sha256_fingerprint(
        srv.leaf_der.as_ref(),
    ));
    assert_eq!(
        provider.bindings.lock().unwrap().as_slice(),
        [Some(expected.clone())]
    );

    // Later requests negotiate up front with a fresh context: no extra 401 round trip.
    let r = t
        .send(get(srv.url("/svn/repo/b")), &CancelToken::new())
        .await
        .expect("preemptive");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(srv.requests().len(), 3);
    assert_eq!(provider.initiated.load(Ordering::SeqCst), 2);
    assert_eq!(provider.bindings.lock().unwrap()[1], Some(expected));
}

#[tokio::test]
async fn bad_mutual_auth_token_rejects_the_response() {
    let (ca, srv) = server(negotiate_handler(true, true, Some(b"forged"))).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider);
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("mutual auth must fail");
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
    assert!(err.message.contains("mutual"), "{}", err.message);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(srv.requests().len(), 2);
}

#[tokio::test]
async fn missing_mutual_token_is_accepted() {
    let (ca, srv) = server(negotiate_handler(false, true, None)).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("accepted");
    assert_eq!(r.status().as_u16(), 200);
    assert!(provider.stepped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejected_ticket_prompts_with_negotiate_failed_and_keeps_three_basic_attempts() {
    let (ca, srv) = server(negotiate_handler(true, false, None)).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    prompter.push_credentials(Some(("alice", "wrong1")));
    prompter.push_credentials(Some(("alice", "wrong2")));
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("third Basic attempt succeeds");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        prompter.reasons.lock().unwrap().as_slice(),
        [
            PromptReason::NegotiateFailed,
            PromptReason::Rejected,
            PromptReason::Rejected
        ]
    );
    let seen = srv.requests();
    // 401, rejected Negotiate, two wrong Basic, right Basic.
    assert_eq!(seen.len(), 5);
    assert!(
        seen[1]
            .header("authorization")
            .unwrap()
            .starts_with("Negotiate ")
    );
    assert!(
        seen[2..]
            .iter()
            .all(|s| s.header("authorization").unwrap().starts_with("Basic "))
    );

    // The host is remembered: the next request does not retry Negotiate.
    t.send(get(srv.url("/b")), &CancelToken::new())
        .await
        .expect("basic preemptive");
    assert_eq!(provider.initiated.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn negotiate_only_server_reports_negotiate_failed() {
    let (ca, srv) = server(negotiate_handler(false, false, None)).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider);
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("no fallback");
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
    assert_eq!(prompter.credential_prompts(), 0);
    assert_eq!(srv.requests().len(), 2);
}

#[tokio::test]
async fn missing_ticket_falls_back_to_prompt_without_sending_a_token() {
    let (ca, srv) = server(negotiate_handler(true, true, None)).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    *provider.fail_initiate.lock().unwrap() = Some("no Kerberos credentials".into());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider);
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("basic");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        prompter.reasons.lock().unwrap().as_slice(),
        [PromptReason::NegotiateFailed]
    );
    let seen = srv.requests();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].header("authorization"), Some(basic("alice", "pw")));
}

#[tokio::test]
async fn continuation_challenge_is_a_failure_not_a_loop() {
    // An NTLM-style second leg: 401 with a Negotiate token. Not supported.
    let handler: Arc<Handler> = Arc::new(|seen| {
        if seen.header("authorization").is_some() {
            Reply::status(401).header("www-authenticate", format!("Negotiate {}", b64(b"leg2")))
        } else {
            Reply::status(401).header("www-authenticate", "Negotiate")
        }
    });
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("unsupported leg");
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
    assert_eq!(srv.requests().len(), 2);
    assert!(provider.stepped.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancel_while_the_provider_blocks_returns_promptly() {
    let (ca, srv) = server(negotiate_handler(true, true, None)).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    *provider.block_for.lock().unwrap() = Duration::from_millis(1500);
    let t = transport_with_negotiate(&ca, prompter, provider);
    let cancel = CancelToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = Instant::now();
    let err = t
        .send(get(srv.url("/")), &cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, ErrorCode::Cancelled);
    // The runtime thread was free to run the cancel timer while initiate blocked.
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(srv.requests().len(), 1);
}

/// The client token carried by a request, if any.
fn sent_token(seen: &common::Seen) -> Option<Vec<u8>> {
    let auth = seen.header("authorization")?;
    STANDARD.decode(auth.strip_prefix("Negotiate ")?).ok()
}

fn numbered_provider() -> Arc<FakeProvider> {
    Arc::new(FakeProvider {
        unique_tokens: true,
        ..FakeProvider::default()
    })
}

#[tokio::test]
async fn plain_http_never_sends_a_negotiate_token() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(401)
                .append_header("www-authenticate", "Negotiate")
                .append_header("www-authenticate", "Basic realm=\"VisualSVN Server\""),
        )
        .mount(&server)
        .await;
    let ca = TestCa::new();
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    let url = url::Url::parse(&format!(
        "{}/svn/repo",
        server.uri().replace("127.0.0.1", "localhost")
    ))
    .unwrap();
    let err = t
        .send(get(url), &CancelToken::new())
        .await
        .expect_err("http must not authenticate");
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
    assert!(err.message.contains("https://"), "{}", err.message);
    assert_eq!(provider.initiated.load(Ordering::SeqCst), 0);
    assert_eq!(prompter.credential_prompts(), 0);
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert!(
        received
            .iter()
            .all(|r| !r.headers.contains_key("authorization"))
    );
}

#[tokio::test]
async fn transient_retry_sends_a_fresh_token_not_a_replay() {
    let seen_auth = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen_auth);
    let handler: Arc<Handler> = Arc::new(move |seen| match sent_token(seen) {
        None => Reply::status(401).header("www-authenticate", "Negotiate"),
        // The first token hits a gateway error; a replayed token would be rejected.
        Some(_) if counter.fetch_add(1, Ordering::SeqCst) == 0 => Reply::status(503),
        Some(t) if t == b"client-ap-req#1" => {
            Reply::status(401).header("www-authenticate", "Negotiate")
        }
        Some(_) => Reply::ok("sso"),
    });
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = numbered_provider();
    let config = svn_ra::transport::TransportConfig {
        proxy: svn_ra::transport::ProxyConfig::Disabled,
        extra_root_certs: vec![ca.der()],
        retry: svn_ra::transport::RetryPolicy {
            delays: vec![Duration::from_millis(10)],
        },
        ..svn_ra::transport::TransportConfig::default()
    };
    let t = svn_ra::transport::HttpTransport::new(
        config,
        prompter.clone(),
        Arc::new(svn_ra::transport::MemoryTrustStore::new()),
    )
    .with_negotiate(Some(provider.clone()));
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("retry with a fresh token");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 0);
    let tokens: Vec<_> = srv.requests().iter().map(sent_token).collect();
    assert_eq!(
        tokens,
        [
            None,
            Some(b"client-ap-req#1".to_vec()),
            Some(b"client-ap-req#2".to_vec())
        ]
    );
}

#[tokio::test]
async fn rejected_up_front_token_retries_one_fresh_context_before_prompting() {
    // Token #2 (the first up-front one) is rejected, e.g. clock skew; every other token works.
    let handler: Arc<Handler> = Arc::new(|seen| match sent_token(seen) {
        Some(t) if t != b"client-ap-req#2" => Reply::ok("sso"),
        _ => Reply::status(401)
            .header("www-authenticate", "Negotiate")
            .header("www-authenticate", "Basic realm=\"VisualSVN Server\""),
    });
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = numbered_provider();
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    t.send(get(srv.url("/a")), &CancelToken::new())
        .await
        .expect("sso");
    let r = t
        .send(get(srv.url("/b")), &CancelToken::new())
        .await
        .expect("fresh context after a rejected up-front token");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 0);
    let tokens: Vec<_> = srv.requests().iter().map(sent_token).collect();
    assert_eq!(
        tokens,
        [
            None,
            Some(b"client-ap-req#1".to_vec()),
            Some(b"client-ap-req#2".to_vec()),
            Some(b"client-ap-req#3".to_vec())
        ]
    );
}

#[tokio::test]
async fn negotiate_failure_memory_expires() {
    let handler = negotiate_handler(false, true, None);
    let (ca, srv) = server(handler).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");

    // Default: the failure is remembered, so a new ticket is not tried within the window.
    let provider = Arc::new(FakeProvider::default());
    *provider.fail_initiate.lock().unwrap() = Some("no Kerberos credentials".into());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    let err = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("no ticket");
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
    let child = err.child.as_ref().expect("reason");
    assert!(
        child.message.contains("no Kerberos credentials"),
        "{}",
        child.message
    );
    *provider.fail_initiate.lock().unwrap() = None;
    t.send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("still remembered");
    assert_eq!(provider.initiated.load(Ordering::SeqCst), 1);

    // Once the memory has expired, the new ticket is used.
    let provider = Arc::new(FakeProvider::default());
    *provider.fail_initiate.lock().unwrap() = Some("no Kerberos credentials".into());
    let config = svn_ra::transport::TransportConfig {
        proxy: svn_ra::transport::ProxyConfig::Disabled,
        extra_root_certs: vec![ca.der()],
        ..svn_ra::transport::TransportConfig::default()
    };
    let t = svn_ra::transport::HttpTransport::new(
        config,
        prompter.clone(),
        Arc::new(svn_ra::transport::MemoryTrustStore::new()),
    )
    .with_negotiate(Some(provider.clone()))
    .with_negotiate_retry_after(Duration::ZERO);
    t.send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect_err("no ticket");
    *provider.fail_initiate.lock().unwrap() = None;
    let r = t
        .send(get(srv.url("/")), &CancelToken::new())
        .await
        .expect("kinit picked up");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(prompter.credential_prompts(), 0);
}

#[tokio::test]
async fn ip_host_is_refused_before_the_provider_is_called() {
    // With Basic offered: the user is prompted, no token is built for `HTTP/127.0.0.1`.
    let (ca, srv) = server(negotiate_handler(true, true, None)).await;
    let prompter = ScriptedPrompter::with_credentials("alice", "pw");
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter.clone(), provider.clone());
    let r = t
        .send(get(srv.ip_url("/")), &CancelToken::new())
        .await
        .expect("basic");
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        prompter.reasons.lock().unwrap().as_slice(),
        [PromptReason::NegotiateFailed]
    );
    assert_eq!(provider.initiated.load(Ordering::SeqCst), 0);

    // Negotiate only: the error says to use the full host name.
    let (ca, srv) = server(negotiate_handler(false, true, None)).await;
    let provider = Arc::new(FakeProvider::default());
    let t = transport_with_negotiate(&ca, prompter, provider.clone());
    let err = t
        .send(get(srv.ip_url("/")), &CancelToken::new())
        .await
        .expect_err("ip");
    assert_eq!(err.code, ErrorCode::NegotiateFailed);
    let child = err.child.as_ref().expect("reason");
    assert!(
        child.message.contains("full host name"),
        "{}",
        child.message
    );
    assert_eq!(provider.initiated.load(Ordering::SeqCst), 0);
}
