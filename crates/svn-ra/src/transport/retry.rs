//! Retry policy and error classification.

use std::time::Duration;

use reqwest::Method;
use svn_types::{ConnectionFailure, ErrorCode, SvnError};

/// Retry settings for idempotent requests.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Delays before each retry; its length is the retry count.
    pub delays: Vec<Duration>,
}

impl Default for RetryPolicy {
    /// Three retries after 0.5 s, 1 s and 2 s.
    fn default() -> Self {
        Self {
            delays: vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ],
        }
    }
}

impl RetryPolicy {
    /// Policy that never retries.
    pub fn none() -> Self {
        Self { delays: Vec::new() }
    }
}

/// Methods that may be repeated safely. Everything else (POST, PUT, MERGE, LOCK, CHECKOUT, ...)
/// is sent at most once per authentication round.
pub fn is_idempotent(method: &Method) -> bool {
    matches!(
        method.as_str(),
        "GET" | "HEAD" | "OPTIONS" | "PROPFIND" | "REPORT"
    )
}

/// Writes that settle an operation (`MERGE` turns the commit transaction into a revision,
/// `LOCK`/`UNLOCK` take effect at once): each goes out on a connection of its own, never on one
/// that already carried requests, so no proxy limit or stale pooled connection can break it.
/// Their outcome would otherwise be unknown, while a broken write before them leaves nothing
/// behind.
pub fn needs_new_connection(method: &Method) -> bool {
    matches!(method.as_str(), "MERGE" | "LOCK" | "UNLOCK")
}

/// Statuses that signal a transient gateway problem worth retrying.
pub(crate) fn is_transient_status(status: u16) -> bool {
    matches!(status, 502..=504)
}

/// Maps an HTTP status to an error class; `None` for statuses the caller interprets itself
/// (2xx, 207, 404/409/412 which mean different things per method).
pub fn classify_status(status: u16) -> Option<ErrorCode> {
    match status {
        401 | 407 => Some(ErrorCode::AuthRequired),
        403 => Some(ErrorCode::Forbidden),
        408 | 504 => Some(ErrorCode::Timeout),
        423 => Some(ErrorCode::LockedByOther),
        500 | 502 | 503 => Some(ErrorCode::Network),
        _ => None,
    }
}

/// Converts a reqwest failure into an engine error. The URL is stripped because it may carry
/// user-supplied path data and must never leak userinfo. A failure of the connection itself is
/// classified in [`SvnError::connection`].
pub(crate) fn classify_error(err: reqwest::Error) -> SvnError {
    let err = err.without_url();
    // No connection was opened, so nothing of the request reached the server.
    let not_sent = err.is_connect();
    let failure = connection_failure(&err);
    let code = if err.is_timeout() || failure == Some(ConnectionFailure::Timeout) {
        ErrorCode::Timeout
    } else if err.is_builder() {
        ErrorCode::Internal
    } else {
        ErrorCode::Network
    };
    let e = SvnError::new(code, format!("HTTP request failed: {}", error_chain(&err)));
    let e = if failure.is_none() && unreachable(&err) {
        e.with_unreachable()
    } else {
        e
    };
    let e = if not_sent { e.with_not_sent() } else { e };
    match failure {
        Some(f) => e.with_connection(f),
        None => e,
    }
}

/// Whether `err` failed to connect because the server could not be reached: DNS, refused,
/// network or host unreachable. A TLS handshake or certificate problem is never unreachable:
/// it must not be waited out.
fn unreachable(err: &reqwest::Error) -> bool {
    use std::io::ErrorKind;
    if !err.is_connect() {
        return false;
    }
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(err);
    let mut found = false;
    while let Some(e) = src {
        if e.downcast_ref::<rustls::Error>().is_some() {
            return false;
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io
                .get_ref()
                .is_some_and(|inner| inner.is::<rustls::Error>())
            {
                return false;
            }
            if matches!(
                io.kind(),
                ErrorKind::ConnectionRefused
                    | ErrorKind::HostUnreachable
                    | ErrorKind::NetworkUnreachable
                    | ErrorKind::NetworkDown
                    | ErrorKind::AddrNotAvailable
            ) {
                found = true;
            }
        }
        // hyper-util's resolver: "dns error: failed to lookup address information".
        let text = e.to_string();
        if text.starts_with("dns error") || text.contains("failed to lookup address") {
            found = true;
        }
        src = e.source();
    }
    found
}

/// How the connection behind `err` failed, from the I/O and hyper errors in its source chain.
/// `None` for anything else (DNS, connection refused, TLS handshake, HTTP parse errors).
pub(crate) fn connection_failure(
    err: &(dyn std::error::Error + 'static),
) -> Option<ConnectionFailure> {
    let mut hyper_hint = None;
    let mut timeout = false;
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = src {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && let Some(f) = io_failure(io)
        {
            return Some(f);
        }
        if let Some(h) = e.downcast_ref::<hyper::Error>() {
            if h.is_timeout() {
                timeout = true;
            } else if h.is_incomplete_message() || h.is_closed() || h.is_body_write_aborted() {
                hyper_hint.get_or_insert(ConnectionFailure::ConnectionClosedByServer);
            }
        }
        if let Some(r) = e.downcast_ref::<reqwest::Error>()
            && r.is_timeout()
        {
            timeout = true;
        }
        src = e.source();
    }
    if timeout {
        return Some(ConnectionFailure::Timeout);
    }
    hyper_hint
}

fn io_failure(io: &std::io::Error) -> Option<ConnectionFailure> {
    use std::io::ErrorKind;
    match io.kind() {
        // rustls: "peer closed connection without sending TLS close_notify".
        ErrorKind::UnexpectedEof if io.to_string().contains("close_notify") => {
            Some(ConnectionFailure::TlsClosedUncleanly)
        }
        ErrorKind::UnexpectedEof | ErrorKind::NotConnected => {
            Some(ConnectionFailure::ConnectionClosedByServer)
        }
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            Some(ConnectionFailure::ConnectionReset)
        }
        ErrorKind::TimedOut => Some(ConnectionFailure::Timeout),
        _ => None,
    }
}

/// Whether a failed exchange may be sent again on a fresh connection: the connection broke
/// (closed, reset, unclean TLS close) or stalled. Errors without a classification (connection
/// refused, DNS, certificate, protocol violations) are not covered.
pub(crate) fn is_connection_failure(err: &SvnError) -> bool {
    err.connection.is_some()
}

/// Joins an error and its sources into one line.
pub(crate) fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut src = err.source();
    while let Some(e) = src {
        out.push_str(": ");
        out.push_str(&e.to_string());
        src = e.source();
    }
    out
}
