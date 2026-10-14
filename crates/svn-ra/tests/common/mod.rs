//! Shared test fixtures: scripted prompter, certificate authority and a scriptable HTTPS server.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose, SanType,
    date_time_ymd,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use secrecy::SecretString;
use svn_ra::transport::{HttpTransport, MemoryTrustStore, TransportConfig};
use svn_ra_api::{AuthMethod, AuthPrompter, CertDecision, CertInfo, Credentials};
use svn_types::OriginKey;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Prompter that replays scripted answers and records every call.
#[derive(Default)]
pub struct ScriptedPrompter {
    pub credential_answers: Mutex<VecDeque<Option<(String, String)>>>,
    /// Used when the queue is empty.
    pub default_credentials: Mutex<Option<(String, String)>>,
    pub credential_calls: Mutex<Vec<(OriginKey, u8, Vec<AuthMethod>)>>,
    pub reasons: Mutex<Vec<svn_ra_api::PromptReason>>,
    pub cert_decision: Mutex<Option<CertDecision>>,
    pub cert_calls: Mutex<Vec<CertInfo>>,
    pub delay: Mutex<Duration>,
    pub concurrent: AtomicUsize,
    pub max_concurrent: AtomicUsize,
    /// Answer of `stored_credentials` (nothing stored by default).
    pub stored: Mutex<Option<(String, String)>>,
    pub stored_calls: AtomicUsize,
    /// Every `stored_credentials_rejected` call.
    pub stored_rejected: Mutex<Vec<svn_ra_api::CredentialRequest>>,
    /// When set, `stored_credentials_rejected` cancels this token and yields before recording,
    /// as if the user cancelled while the hook ran.
    pub cancel_in_rejected_hook: Mutex<Option<svn_types::CancelToken>>,
    /// `save` flag of the prompt answers.
    pub save_answers: std::sync::atomic::AtomicBool,
    /// Every `credentials_accepted` call: origin, user name, password and `save`.
    pub accepted: Mutex<Vec<(OriginKey, String, String, bool)>>,
}

impl ScriptedPrompter {
    pub fn with_credentials(user: &str, pass: &str) -> Arc<Self> {
        let p = Self::default();
        *p.default_credentials.lock().unwrap() = Some((user.into(), pass.into()));
        Arc::new(p)
    }

    pub fn with_cert_decision(decision: CertDecision) -> Arc<Self> {
        let p = Self::default();
        *p.cert_decision.lock().unwrap() = Some(decision);
        Arc::new(p)
    }

    pub fn push_credentials(&self, answer: Option<(&str, &str)>) {
        self.credential_answers
            .lock()
            .unwrap()
            .push_back(answer.map(|(u, p)| (u.to_owned(), p.to_owned())));
    }

    /// Prompter with `user`/`pass` in its credential store and nothing to answer a prompt.
    pub fn with_stored(user: &str, pass: &str) -> Arc<Self> {
        let p = Self::default();
        *p.stored.lock().unwrap() = Some((user.into(), pass.into()));
        Arc::new(p)
    }

    pub fn stored_rejections(&self) -> usize {
        self.stored_rejected.lock().unwrap().len()
    }

    pub fn credential_prompts(&self) -> usize {
        self.credential_calls.lock().unwrap().len()
    }

    pub fn accepted(&self) -> Vec<(OriginKey, String, String, bool)> {
        self.accepted.lock().unwrap().clone()
    }

    pub fn cert_prompts(&self) -> usize {
        self.cert_calls.lock().unwrap().len()
    }
}

#[async_trait]
impl AuthPrompter for ScriptedPrompter {
    async fn credentials(&self, req: &svn_ra_api::CredentialRequest) -> Option<Credentials> {
        let (origin, attempt, offered) = (&req.origin, req.attempt as u8, &req.offered);
        self.reasons.lock().unwrap().push(req.reason);
        let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now, Ordering::SeqCst);
        self.credential_calls
            .lock()
            .unwrap()
            .push((origin.clone(), attempt, offered.clone()));
        let delay = *self.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        let answer = {
            let mut q = self.credential_answers.lock().unwrap();
            match q.pop_front() {
                Some(a) => a,
                None => self.default_credentials.lock().unwrap().clone(),
            }
        };
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
        let save = self.save_answers.load(Ordering::SeqCst);
        answer.map(|(username, password)| Credentials {
            username,
            password: SecretString::from(password),
            save,
            method: AuthMethod::Basic,
        })
    }

    async fn stored_credentials(
        &self,
        _req: &svn_ra_api::CredentialRequest,
    ) -> Option<Credentials> {
        self.stored_calls.fetch_add(1, Ordering::SeqCst);
        self.stored
            .lock()
            .unwrap()
            .clone()
            .map(|(username, password)| Credentials {
                username,
                password: SecretString::from(password),
                save: true,
                method: AuthMethod::Basic,
            })
    }

    async fn stored_credentials_rejected(&self, req: &svn_ra_api::CredentialRequest) {
        let cancel = self.cancel_in_rejected_hook.lock().unwrap().clone();
        if let Some(cancel) = cancel {
            cancel.cancel();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.stored_rejected.lock().unwrap().push(req.clone());
    }

    async fn credentials_accepted(&self, origin: &OriginKey, credentials: &Credentials) {
        use secrecy::ExposeSecret as _;
        self.accepted.lock().unwrap().push((
            origin.clone(),
            credentials.username.clone(),
            credentials.password.expose_secret().to_owned(),
            credentials.save,
        ));
    }

    async fn confirm_certificate(&self, info: &CertInfo) -> CertDecision {
        self.cert_calls.lock().unwrap().push(info.clone());
        self.cert_decision
            .lock()
            .unwrap()
            .unwrap_or(CertDecision::Reject)
    }
}

/// Test certificate authority.
pub struct TestCa {
    pub issuer: rcgen::CertifiedIssuer<'static, KeyPair>,
}

impl TestCa {
    pub fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "OpenSVN Test CA");
        let key = KeyPair::generate().expect("key");
        let issuer = rcgen::CertifiedIssuer::self_signed(params, key).expect("ca cert");
        Self { issuer }
    }

    pub fn der(&self) -> CertificateDer<'static> {
        self.issuer.as_ref().der().clone()
    }

    /// Leaf signed by this CA. `names` become DNS SANs; loopback IPs are always added.
    pub fn leaf(&self, names: &[&str], expired: bool) -> Leaf {
        let mut params =
            CertificateParams::new(names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .expect("params");
        params
            .subject_alt_names
            .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
        params
            .distinguished_name
            .push(DnType::CommonName, "svn.test.local");
        if expired {
            params.not_before = date_time_ymd(2020, 1, 1);
            params.not_after = date_time_ymd(2021, 1, 1);
        }
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.issuer).expect("leaf");
        Leaf {
            cert_der: cert.der().clone(),
            key_der: key.serialize_der(),
        }
    }

    /// Self-signed leaf that is not issued by any CA.
    pub fn self_signed_leaf(names: &[&str]) -> Leaf {
        let mut params =
            CertificateParams::new(names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .expect("params");
        params
            .subject_alt_names
            .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
        let key = KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("self-signed");
        Leaf {
            cert_der: cert.der().clone(),
            key_der: key.serialize_der(),
        }
    }
}

pub struct Leaf {
    pub cert_der: CertificateDer<'static>,
    pub key_der: Vec<u8>,
}

/// What the server saw.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body_len: u64,
    pub body_head: Bytes,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }
}

/// Response body: a fixed buffer or `n` zero bytes produced on demand.
pub enum TestBody {
    Full(Option<Bytes>),
    Zeros { remaining: u64 },
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match &mut *self {
            TestBody::Full(b) => Poll::Ready(b.take().map(|b| Ok(Frame::data(b)))),
            TestBody::Zeros { remaining } => {
                if *remaining == 0 {
                    return Poll::Ready(None);
                }
                let n = (*remaining).min(64 * 1024);
                *remaining -= n;
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(vec![0u8; n as usize])))))
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            TestBody::Full(Some(b)) => SizeHint::with_exact(b.len() as u64),
            TestBody::Full(None) => SizeHint::with_exact(0),
            TestBody::Zeros { remaining } => SizeHint::with_exact(*remaining),
        }
    }
}

/// Scripted answer.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: TestBody,
    pub delay: Duration,
}

impl Reply {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: TestBody::Full(Some(Bytes::new())),
            delay: Duration::ZERO,
        }
    }

    pub fn ok(body: &'static str) -> Self {
        let mut r = Self::status(200);
        r.body = TestBody::Full(Some(Bytes::from_static(body.as_bytes())));
        r
    }

    pub fn unauthorized(realm: &str) -> Self {
        let mut r = Self::status(401);
        r.headers
            .push(("www-authenticate", format!("Basic realm=\"{realm}\"")));
        r
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn zeros(n: u64) -> Self {
        let mut r = Self::status(200);
        r.body = TestBody::Zeros { remaining: n };
        r
    }
}

pub type Handler = dyn Fn(&Seen) -> Reply + Send + Sync;

/// HTTPS server on 127.0.0.1 with a scripted handler.
pub struct TlsServer {
    pub addr: SocketAddr,
    pub leaf_der: CertificateDer<'static>,
    pub log: Arc<Mutex<Vec<Seen>>>,
    pub connections: Arc<AtomicUsize>,
}

impl TlsServer {
    pub async fn start(leaf: &Leaf, handler: Arc<Handler>) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.cert_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key_der.clone())),
            )
            .expect("server cert");
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let connections = Arc::new(AtomicUsize::new(0));
        let (log2, conns2) = (Arc::clone(&log), Arc::clone(&connections));
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                conns2.fetch_add(1, Ordering::SeqCst);
                let acceptor = acceptor.clone();
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log2);
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let handler = Arc::clone(&handler);
                        let log = Arc::clone(&log);
                        async move { Ok::<_, hyper::Error>(serve(req, &handler, &log).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), svc)
                        .await;
                });
            }
        });
        Self {
            addr,
            leaf_der: leaf.cert_der.clone(),
            log,
            connections,
        }
    }

    pub fn url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("https://localhost:{}{path}", self.addr.port())).expect("url")
    }

    pub fn ip_url(&self, path: &str) -> url::Url {
        url::Url::parse(&format!("https://127.0.0.1:{}{path}", self.addr.port())).expect("url")
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }
}

async fn serve(
    req: Request<Incoming>,
    handler: &Arc<Handler>,
    log: &Arc<Mutex<Vec<Seen>>>,
) -> Response<TestBody> {
    let (parts, mut body) = req.into_parts();
    let mut body_len = 0u64;
    let mut head = Vec::new();
    while let Some(Ok(frame)) = body.frame().await {
        if let Some(data) = frame.data_ref() {
            body_len += data.len() as u64;
            if head.len() < 64 * 1024 {
                head.extend_from_slice(data);
            }
        }
    }
    let seen = Seen {
        method: parts.method,
        path: parts.uri.to_string(),
        headers: parts.headers,
        body_len,
        body_head: Bytes::from(head),
    };
    log.lock().unwrap().push(seen.clone());
    let reply = handler(&seen);
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    let mut resp = Response::new(reply.body);
    *resp.status_mut() = StatusCode::from_u16(reply.status).expect("status");
    for (k, v) in reply.headers {
        resp.headers_mut()
            .append(k, v.parse().expect("header value"));
    }
    resp
}

/// Transport that trusts `ca` through the extra-roots path.
pub fn transport_trusting(
    ca: &TestCa,
    prompter: Arc<dyn AuthPrompter>,
    tweak: impl FnOnce(&mut TransportConfig),
) -> Arc<HttpTransport> {
    let mut config = TransportConfig {
        proxy: svn_ra::transport::ProxyConfig::Disabled,
        extra_root_certs: vec![ca.der()],
        ..TransportConfig::default()
    };
    tweak(&mut config);
    // Deterministic regardless of the build's Negotiate support or the host's Kerberos tickets.
    Arc::new(
        HttpTransport::new(config, prompter, Arc::new(MemoryTrustStore::new()))
            .with_negotiate(None),
    )
}

/// Like [`transport_trusting`] with a scripted Negotiate provider.
pub fn transport_with_negotiate(
    ca: &TestCa,
    prompter: Arc<dyn AuthPrompter>,
    provider: Arc<dyn svn_ra::auth::NegotiateProvider>,
) -> Arc<HttpTransport> {
    let config = TransportConfig {
        proxy: svn_ra::transport::ProxyConfig::Disabled,
        extra_root_certs: vec![ca.der()],
        ..TransportConfig::default()
    };
    Arc::new(
        HttpTransport::new(config, prompter, Arc::new(MemoryTrustStore::new()))
            .with_negotiate(Some(provider)),
    )
}

/// Expected `Authorization` header for `user:pass`.
pub fn basic(user: &str, pass: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

/// Handler that requires Basic `user:pass` in `realm`.
pub fn basic_auth_handler(
    realm: &'static str,
    user: &'static str,
    pass: &'static str,
) -> Arc<Handler> {
    Arc::new(move |seen: &Seen| {
        if seen.header("authorization").as_deref() == Some(basic(user, pass).as_str()) {
            Reply::ok("secret")
        } else {
            Reply::unauthorized(realm)
        }
    })
}
