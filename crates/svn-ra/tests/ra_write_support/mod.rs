//! Shared helpers for the update, commit and remote-edit tests: a scripted `mod_dav_svn` front
//! (wiremock), a transport without proxy or retry delays, and request inspection.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use secrecy::SecretString;
use svn_ra::ops::update::RaWire;
use svn_ra::transport::{
    HttpTransport, MemoryTrustStore, ProxyConfig, RetryPolicy, TransportConfig,
};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertInfo, CredentialRequest, Credentials,
};
use svn_types::{CancelToken, RepoUrl};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Repository root path on the mock server.
pub const ROOT: &str = "/svn/repo";

/// How the mock announces bulk updates.
#[derive(Clone, Copy)]
pub enum Bulk {
    /// `SVN-Allow-Bulk-Updates: Prefer`.
    Prefer,
    /// `SVN-Allow-Bulk-Updates: Off`.
    Off,
    /// Header not sent.
    Absent,
}

/// Prompter that answers every credential request with fixed credentials and counts them.
#[derive(Default)]
pub struct FixedPrompter {
    pub prompts: AtomicUsize,
}

#[async_trait]
impl AuthPrompter for FixedPrompter {
    async fn credentials(&self, _req: &CredentialRequest) -> Option<Credentials> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        Some(Credentials {
            username: "alice".into(),
            password: SecretString::from("secret".to_owned()),
            save: false,
            method: AuthMethod::Basic,
        })
    }

    async fn confirm_certificate(&self, _info: &CertInfo) -> CertDecision {
        CertDecision::Reject
    }
}

/// Mounts the `OPTIONS` answer of a 1.15 server (HTTPv2, inline props, svndiff 1/2).
pub async fn server(bulk: Bulk) -> MockServer {
    let server = MockServer::start().await;
    let mut t = ResponseTemplate::new(200)
        .append_header("DAV", "1,2")
        .append_header("DAV", "http://subversion.tigris.org/xmlns/dav/svn/depth")
        .append_header(
            "DAV",
            "http://subversion.tigris.org/xmlns/dav/svn/log-revprops",
        )
        .append_header(
            "DAV",
            "http://subversion.tigris.org/xmlns/dav/svn/inline-props",
        )
        .append_header(
            "DAV",
            "http://subversion.tigris.org/xmlns/dav/svn/ephemeral-txnprops",
        )
        .append_header("DAV", "http://subversion.tigris.org/xmlns/dav/svn/svndiff1")
        .append_header(
            "DAV",
            "http://subversion.tigris.org/xmlns/dav/svn/put-result-checksum",
        )
        .append_header("SVN-Youngest-Rev", "5")
        .append_header(
            "SVN-Repository-UUID",
            "7462d98e-4071-4508-b797-d2922b5cda8f",
        )
        .append_header("SVN-Repository-Root", ROOT)
        .append_header("SVN-Me-Resource", "/svn/repo/!svn/me")
        .append_header("SVN-Rev-Root-Stub", "/svn/repo/!svn/rvr")
        .append_header("SVN-Rev-Stub", "/svn/repo/!svn/rev")
        .append_header("SVN-Txn-Root-Stub", "/svn/repo/!svn/txr")
        .append_header("SVN-Txn-Stub", "/svn/repo/!svn/txn")
        .append_header("SVN-VTxn-Root-Stub", "/svn/repo/!svn/vtxr")
        .append_header("SVN-VTxn-Stub", "/svn/repo/!svn/vtxn")
        .append_header("SVN-Supported-Posts", "create-txn")
        .append_header("SVN-Supported-Posts", "create-txn-with-props");
    t = match bulk {
        Bulk::Prefer => t.append_header("SVN-Allow-Bulk-Updates", "Prefer"),
        Bulk::Off => t.append_header("SVN-Allow-Bulk-Updates", "Off"),
        Bulk::Absent => t,
    };
    Mock::given(method("OPTIONS"))
        .respond_with(t)
        .mount(&server)
        .await;
    server
}

/// URL of the session (`<server>/svn/repo/trunk`).
pub fn session_url(server: &MockServer) -> RepoUrl {
    RepoUrl::parse(&format!("{}{ROOT}/trunk", server.uri())).expect("session url")
}

/// Transport without proxy and with fast retries.
pub fn transport(prompter: Arc<dyn AuthPrompter>) -> Arc<HttpTransport> {
    let config = TransportConfig {
        proxy: ProxyConfig::Disabled,
        retry: RetryPolicy::none(),
        ..TransportConfig::default()
    };
    Arc::new(HttpTransport::new(
        config,
        prompter,
        Arc::new(MemoryTrustStore::new()),
    ))
}

/// Opens the wiring against the mock (this issues the `OPTIONS`).
pub async fn wire(server: &MockServer, connections: usize) -> RaWire {
    RaWire::open(
        transport(Arc::new(FixedPrompter::default())),
        session_url(server),
        &CancelToken::new(),
    )
    .await
    .expect("open wire")
    .with_connections(connections)
}

/// Bytes of a recorded svn-protocol fixture body.
pub fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../svn-protocol/tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
}

/// `(method, path)` of every request except `OPTIONS`, in arrival order.
pub async fn requests(server: &MockServer) -> Vec<(String, String)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() != "OPTIONS")
        .map(|r| (r.method.to_string(), r.url.path().to_owned()))
        .collect()
}
