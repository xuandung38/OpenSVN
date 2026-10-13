//! Transport configuration.

use std::time::Duration;

use rustls::pki_types::CertificateDer;
use secrecy::SecretString;

use super::retry::RetryPolicy;

/// Smallest allowed connections per host.
pub const MIN_CONNECTIONS: usize = 1;
/// Largest allowed connections per host (same bound as serf).
pub const MAX_CONNECTIONS: usize = 8;
/// Default connections per host.
pub const DEFAULT_CONNECTIONS: usize = 4;

/// Proxy selection.
#[derive(Clone, Default)]
pub enum ProxyConfig {
    /// Use the operating system proxy settings (default).
    #[default]
    System,
    /// Connect directly.
    Disabled,
    /// Use a fixed proxy for every request. Proxy credentials are sent (Basic) only because the
    /// user configured them explicitly, even for an `http://` proxy; a 407 from an unconfigured
    /// proxy is never answered with a prompt. Credentials for the target server are unaffected
    /// and never go over plain http.
    Manual {
        /// Proxy URL, for example `http://proxy.test.local:3128`.
        url: String,
        /// Optional proxy credentials (Basic).
        credentials: Option<(String, SecretString)>,
    },
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::Disabled => f.write_str("Disabled"),
            // The URL may embed userinfo and the password is secret: print neither.
            Self::Manual { .. } => f.write_str("Manual(..)"),
        }
    }
}

/// Settings for [`super::HttpTransport`].
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Maximum concurrent connections per host; clamped to 1..=8.
    pub max_connections_per_host: usize,
    /// TCP + TLS connect timeout.
    pub connect_timeout: Duration,
    /// Maximum idle time while waiting for response bytes (headers or body chunks).
    pub read_timeout: Duration,
    /// Retry behavior for idempotent requests.
    pub retry: RetryPolicy,
    /// Maximum same-origin redirects followed per request.
    pub max_redirects: u8,
    /// Proxy selection.
    pub proxy: ProxyConfig,
    /// Additional trusted root certificates (manual CA import), DER encoded.
    pub extra_root_certs: Vec<CertificateDer<'static>>,
    /// `User-Agent` header value.
    pub user_agent: String,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            max_connections_per_host: DEFAULT_CONNECTIONS,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            retry: RetryPolicy::default(),
            max_redirects: 5,
            proxy: ProxyConfig::System,
            extra_root_certs: Vec::new(),
            user_agent: concat!("OpenSVN/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }
}

impl TransportConfig {
    /// Connection limit after clamping to the supported range.
    pub fn effective_connections(&self) -> usize {
        self.max_connections_per_host
            .clamp(MIN_CONNECTIONS, MAX_CONNECTIONS)
    }
}
