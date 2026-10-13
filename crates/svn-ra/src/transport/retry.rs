//! Retry policy and error classification.

use std::time::Duration;

use reqwest::Method;
use svn_types::{ErrorCode, SvnError};

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
/// user-supplied path data and must never leak userinfo.
pub(crate) fn classify_error(err: reqwest::Error) -> SvnError {
    let err = err.without_url();
    let code = if err.is_timeout() {
        ErrorCode::Timeout
    } else if err.is_builder() {
        ErrorCode::Internal
    } else {
        ErrorCode::Network
    };
    SvnError::new(code, format!("HTTP request failed: {}", error_chain(&err)))
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
