//! A scripted HTTPS Subversion server (HTTP protocol v2 `OPTIONS` answer, Basic authentication,
//! path-based authorization and a canned update report) for the connector and broker tests.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Repository root path.
pub(super) const ROOT: &str = "/svn/repo";
pub(super) const REPO_UUID: &str = "7462d98e-4071-4508-b797-d2922b5cda8f";
/// Root of the second repository ([`Script::second_repo`]); [`ROOT`] is a prefix of it.
pub(super) const ROOT2: &str = "/svn/repo2";
pub(super) const REPO2_UUID: &str = "0b8f3c5e-58a1-4c55-9d7e-2b1f0a6c9e41";
pub(super) const REALM: &str = "OpenSVN test";
pub(super) const USER: &str = "alice";
pub(super) const PASSWORD: &str = "s3cret";
/// `Basic base64("alice:s3cret")`.
const GOOD_AUTH: &str = "Basic YWxpY2U6czNjcmV0";

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub(super) struct Seen {
    pub(super) method: Method,
    pub(super) path: String,
    pub(super) authorization: Option<String>,
}

/// What the server enforces.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Script {
    /// Requests need `alice`/`s3cret` (Basic).
    pub(super) require_auth: bool,
    /// Path-based authorization: everything outside `/trunk` (and the `!svn` resources) is
    /// forbidden, like `[/] * =` plus `[/trunk] alice = rw`.
    pub(super) trunk_only: bool,
    /// Also serve a second repository at [`ROOT2`].
    pub(super) second_repo: bool,
    /// Repository names ignore ASCII case, like VisualSVN on Windows: `/svn/REPO/trunk` is in
    /// the repository announced as [`ROOT`].
    pub(super) ignore_case: bool,
}

/// Running server.
pub(super) struct SvnServer {
    pub(super) port: u16,
    pub(super) leaf_der: CertificateDer<'static>,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl SvnServer {
    pub(super) async fn start(script: Script) -> Self {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params
            .subject_alt_names
            .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let leaf_der = cert.der().clone();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf_der.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let log2 = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let log = Arc::clone(&log2);
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let log = Arc::clone(&log);
                        async move { Ok::<_, Infallible>(serve(req, script, &log).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), svc)
                        .await;
                });
            }
        });
        Self {
            port,
            leaf_der,
            log,
        }
    }

    /// `https://127.0.0.1:<port><ROOT><path>`.
    pub(super) fn url(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{ROOT}{path}", self.port)
    }

    pub(super) fn requests(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }

    /// `https://127.0.0.1:<port><path>` (no repository prefix).
    pub(super) fn raw_url(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.port)
    }

    /// `OPTIONS` requests the server answered with 200 (session opens and authentication
    /// checks), by path.
    pub(super) fn options_paths(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|s| s.method == Method::OPTIONS)
            .map(|s| s.path)
            .collect()
    }
}

fn reply(status: u16) -> hyper::http::response::Builder {
    Response::builder().status(status)
}

fn under(path: &str, root: &str, fold: bool) -> bool {
    let head_ok = if fold {
        path.len() >= root.len()
            && path.is_char_boundary(root.len())
            && path[..root.len()].eq_ignore_ascii_case(root)
    } else {
        path.starts_with(root)
    };
    head_ok && (path.len() == root.len() || path.as_bytes()[root.len()] == b'/')
}

/// The repository (root, UUID) serving `path`, in the server's spelling.
fn repository(path: &str, script: Script) -> Option<(&'static str, &'static str)> {
    let mut repos = vec![(ROOT, REPO_UUID)];
    if script.second_repo {
        repos.push((ROOT2, REPO2_UUID));
    }
    repos
        .into_iter()
        .find(|(root, _)| under(path, root, script.ignore_case))
}

fn allowed(path: &str, script: Script) -> bool {
    if !script.trunk_only {
        return true;
    }
    let trunk = format!("{ROOT}/trunk");
    path == trunk
        || path.starts_with(&format!("{trunk}/"))
        || path.starts_with(&format!("{ROOT}/!svn/"))
}

async fn serve(
    req: Request<Incoming>,
    script: Script,
    log: &Mutex<Vec<Seen>>,
) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let authorization = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let path = parts.uri.path().to_owned();
    let authorized = !script.require_auth || authorization.as_deref() == Some(GOOD_AUTH);
    if authorized {
        log.lock().unwrap().push(Seen {
            method: parts.method.clone(),
            path: path.clone(),
            authorization: authorization.clone(),
        });
    } else {
        log.lock().unwrap().push(Seen {
            method: Method::from_bytes(b"UNAUTHORIZED").unwrap(),
            path: path.clone(),
            authorization,
        });
        return reply(401)
            .header("www-authenticate", format!("Basic realm=\"{REALM}\""))
            .body(Full::new(Bytes::new()))
            .unwrap();
    }
    if !allowed(&path, script) {
        return reply(403).body(Full::new(Bytes::new())).unwrap();
    }
    match parts.method.as_str() {
        "OPTIONS" => match repository(&path, script) {
            Some((root, uuid)) => options_reply(root, uuid),
            None => reply(404).body(Full::new(Bytes::new())).unwrap(),
        },
        "REPORT" if path == format!("{ROOT}/!svn/me") => {
            let checkout = String::from_utf8_lossy(&body).contains("start-empty=\"true\"");
            let body = if checkout {
                Bytes::from(fixture("update/checkout-send-all.resp"))
            } else {
                Bytes::from_static(UP_TO_DATE.as_bytes())
            };
            reply(200)
                .header("content-type", "text/xml")
                .body(Full::new(body))
                .unwrap()
        }
        _ => reply(405).body(Full::new(Bytes::new())).unwrap(),
    }
}

/// Update report for a working copy that is already at revision 5.
const UP_TO_DATE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<S:update-report xmlns:S="svn:" xmlns:V="http://subversion.tigris.org/xmlns/dav/" xmlns:D="DAV:" send-all="true" inline-props="true">
<S:target-revision rev="5"/>
<S:open-directory rev="5">
</S:open-directory>
</S:update-report>
"#;

fn options_reply(root: &str, uuid: &str) -> Response<Full<Bytes>> {
    let mut b = reply(200);
    for dav in [
        "1,2",
        "http://subversion.tigris.org/xmlns/dav/svn/depth",
        "http://subversion.tigris.org/xmlns/dav/svn/log-revprops",
        "http://subversion.tigris.org/xmlns/dav/svn/inline-props",
        "http://subversion.tigris.org/xmlns/dav/svn/svndiff1",
    ] {
        b = b.header("DAV", dav);
    }
    b.header("SVN-Youngest-Rev", "5")
        .header("SVN-Repository-UUID", uuid)
        .header("SVN-Repository-Root", root)
        .header("SVN-Me-Resource", format!("{root}/!svn/me"))
        .header("SVN-Rev-Root-Stub", format!("{root}/!svn/rvr"))
        .header("SVN-Rev-Stub", format!("{root}/!svn/rev"))
        .header("SVN-Txn-Root-Stub", format!("{root}/!svn/txr"))
        .header("SVN-Txn-Stub", format!("{root}/!svn/txn"))
        .header("SVN-VTxn-Root-Stub", format!("{root}/!svn/vtxr"))
        .header("SVN-VTxn-Stub", format!("{root}/!svn/vtxn"))
        .header("SVN-Allow-Bulk-Updates", "Prefer")
        .body(Full::new(Bytes::new()))
        .unwrap()
}

/// A recorded protocol fixture of `svn-protocol`.
fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../crates/svn-protocol/tests/data")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
}
