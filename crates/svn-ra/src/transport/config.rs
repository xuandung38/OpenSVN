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

/// Default time an idle pooled connection is kept for reuse. Shorter than the keep-alive timeout
/// of common servers (Apache 5 s, IIS 120 s, many proxies 5-60 s), so the client drops an idle
/// connection before the server does and rarely sends a request into a closing connection.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(4);

/// Default longest idle time after which a connection still carries a write. A write is never
/// sent twice, so it must not go out on a connection that a server, proxy or NAT may have
/// dropped while it was idle: past this window it opens a new one. Writes of one commit follow
/// each other closely, so they still share connections.
pub const DEFAULT_WRITE_REUSE_WINDOW: Duration = Duration::from_secs(1);

/// Default most requests a connection that carries writes is used for, the write included.
/// Some proxies and middleboxes reset a connection under its (N+1)-th request instead of
/// closing it cleanly; a write that meets such a reset is never sent again, so the commit would
/// fail at the same place every time. Two keeps every write on a connection's first or second
/// request. New connections resume the TLS session (no full handshake), so the cost is mostly
/// round trips: against Apache 2.4 + mod_dav_svn 1.14.3 and 1.15.0, a 1000-file commit took
/// 2-6 % longer on a local link and about 40 % longer with 10 ms of round-trip time, where every
/// second write waits for a TCP and a TLS round trip (`crates/svn-ra/tests/commit_timing.rs`).
/// Raise it, or set `0`, where no such middlebox sits on the path.
pub const DEFAULT_WRITE_CONNECTION_REQUESTS: usize = 2;

/// Subversion version this client speaks as (the `User-Agent` prefix and the transaction's
/// client compatibility property).
pub const SVN_COMPAT_VERSION: &str = "1.15.0";

/// `User-Agent` of every request and the transaction's `svn:txn-user-agent`, shaped like
/// TortoiseSVN's (`SVN/<version> (<os>) <client>`). mod_dav_svn treats a client as an svn client
/// only when the agent starts with `SVN/` (1.8 to 1.15); only then does a `LOCK` honour
/// `lock-steal` and an `UNLOCK` honour `lock-break` instead of failing with 423.
pub const USER_AGENT: &str = user_agent!();

macro_rules! user_agent {
    () => {
        concat!(
            "SVN/1.15.0 (",
            os_name!(),
            ") OpenSVN/",
            env!("CARGO_PKG_VERSION")
        )
    };
}
use user_agent;

#[cfg(target_os = "windows")]
macro_rules! os_name {
    () => {
        "Windows"
    };
}
#[cfg(target_os = "macos")]
macro_rules! os_name {
    () => {
        "macOS"
    };
}
#[cfg(target_os = "linux")]
macro_rules! os_name {
    () => {
        "Linux"
    };
}
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
macro_rules! os_name {
    () => {
        "Unix"
    };
}
use os_name;

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
    /// How long an idle connection stays in the pool; see [`DEFAULT_POOL_IDLE_TIMEOUT`].
    pub pool_idle_timeout: Duration,
    /// Longest idle time of a connection reused for a write (any method but `GET`, `HEAD`,
    /// `OPTIONS`, `PROPFIND`, `REPORT`); see [`DEFAULT_WRITE_REUSE_WINDOW`]. Zero sends every
    /// write on a new connection.
    pub write_reuse_window: Duration,
    /// Most requests a connection that carries writes is used for, the write included; see
    /// [`DEFAULT_WRITE_CONNECTION_REQUESTS`]. `0` sets no limit. The final write of an
    /// operation (`MERGE`, `LOCK`, `UNLOCK`) always goes out on a new connection.
    pub write_connection_requests: usize,
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
            pool_idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
            write_reuse_window: DEFAULT_WRITE_REUSE_WINDOW,
            write_connection_requests: DEFAULT_WRITE_CONNECTION_REQUESTS,
            max_redirects: 5,
            proxy: ProxyConfig::System,
            extra_root_certs: Vec::new(),
            user_agent: USER_AGENT.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_is_an_svn_client_agent_of_the_compat_version() {
        // mod_dav_svn only honours lock-steal / lock-break for agents starting with "SVN/".
        assert!(
            USER_AGENT.starts_with(&format!("SVN/{SVN_COMPAT_VERSION} (")),
            "{USER_AGENT}"
        );
        assert!(USER_AGENT.ends_with(concat!(") OpenSVN/", env!("CARGO_PKG_VERSION"))));
        assert_eq!(TransportConfig::default().user_agent, USER_AGENT);
    }
}
