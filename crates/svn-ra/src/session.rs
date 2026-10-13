//! Shared state of one repository session: base URL, transport and the capabilities cache.
//!
//! `RaSessionCore` is what the operation modules (`ops/*`) build on. It owns no protocol parsing;
//! it only issues the initial `OPTIONS` and caches the raw capability headers for the protocol
//! layer to interpret.

use std::sync::Arc;

use reqwest::Method;
use reqwest::header::HeaderMap;
use svn_types::{CancelToken, ErrorCode, RepoUrl, Result, SvnError};
use tokio::sync::OnceCell;

use crate::transport::{HttpRequest, HttpTransport};

/// Raw response headers of the session-opening `OPTIONS` request.
#[derive(Debug, Clone)]
pub struct ServerCapabilities {
    /// All response headers; capability tokens live in `DAV` and `SVN-*` headers.
    pub headers: HeaderMap,
}

impl ServerCapabilities {
    /// Every value of a header, split on commas and trimmed, in server order.
    pub fn tokens(&self, name: &str) -> Vec<String> {
        self.headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect()
    }
}

/// Repository base URL plus the transport used to reach it.
#[derive(Debug)]
pub struct RaSessionCore {
    base: RepoUrl,
    transport: Arc<HttpTransport>,
    capabilities: OnceCell<ServerCapabilities>,
}

impl RaSessionCore {
    /// Creates a session; no network access happens until [`Self::capabilities`] or a request.
    pub fn new(base: RepoUrl, transport: Arc<HttpTransport>) -> Self {
        Self {
            base,
            transport,
            capabilities: OnceCell::new(),
        }
    }

    /// Repository base URL.
    pub fn base(&self) -> &RepoUrl {
        &self.base
    }

    /// The transport shared by all requests of this session.
    pub fn transport(&self) -> &Arc<HttpTransport> {
        &self.transport
    }

    /// Capabilities from `OPTIONS`, fetched once. This is also the first request of a session,
    /// so authentication and certificate decisions happen here before any large upload.
    pub async fn capabilities(&self, cancel: &CancelToken) -> Result<&ServerCapabilities> {
        self.capabilities
            .get_or_try_init(|| async {
                let req = HttpRequest::new(Method::OPTIONS, self.base.as_url().clone());
                let resp = self.transport.send(req, cancel).await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(SvnError::new(
                        ErrorCode::Network,
                        format!("OPTIONS on the repository returned HTTP {status}"),
                    ));
                }
                Ok(ServerCapabilities {
                    headers: resp.headers().clone(),
                })
            })
            .await
    }
}
