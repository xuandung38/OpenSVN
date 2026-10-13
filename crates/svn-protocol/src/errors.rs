//! Error bodies (`D:error` / `m:human-readable@errcode`) and HTTP-status fallback.
//!
//! mod_dav_svn answers failures with
//! `<D:error><C:error/><m:human-readable errcode="160013">text</m:human-readable></D:error>`.
//! Several `human-readable` elements form a cause chain (outermost first). Anything that is not
//! such a document (proxy or IIS error pages, empty bodies) falls back to the HTTP status.

use svn_types::{ErrorCode, SvnError};

use crate::http::Headers;
use crate::xml::{Ns, XmlEvent, XmlReader};

/// Largest error body inspected; longer bodies are cut and normally fall back to the status.
const MAX_ERROR_BODY: usize = 1 << 20;
/// Longest chain of nested errors accepted.
const MAX_CHAIN: usize = 32;

/// Subversion error codes (`svn_error_codes.h`, base 120000 + category * 5000 + offset) that the
/// engine classifies. Values were generated from the 1.15.0 header.
pub mod codes {
    /// `SVN_ERR_FS_NOT_FOUND`.
    pub const FS_NOT_FOUND: u32 = 160_013;
    /// `SVN_ERR_FS_NO_SUCH_REVISION`.
    pub const FS_NO_SUCH_REVISION: u32 = 160_006;
    /// `SVN_ERR_FS_CONFLICT`.
    pub const FS_CONFLICT: u32 = 160_024;
    /// `SVN_ERR_FS_TXN_OUT_OF_DATE`.
    pub const FS_TXN_OUT_OF_DATE: u32 = 160_028;
    /// `SVN_ERR_FS_NO_USER`.
    pub const FS_NO_USER: u32 = 160_034;
    /// `SVN_ERR_FS_PATH_ALREADY_LOCKED`.
    pub const FS_PATH_ALREADY_LOCKED: u32 = 160_035;
    /// `SVN_ERR_FS_PATH_NOT_LOCKED`.
    pub const FS_PATH_NOT_LOCKED: u32 = 160_036;
    /// `SVN_ERR_FS_BAD_LOCK_TOKEN`.
    pub const FS_BAD_LOCK_TOKEN: u32 = 160_037;
    /// `SVN_ERR_FS_NO_LOCK_TOKEN`.
    pub const FS_NO_LOCK_TOKEN: u32 = 160_038;
    /// `SVN_ERR_FS_LOCK_OWNER_MISMATCH`.
    pub const FS_LOCK_OWNER_MISMATCH: u32 = 160_039;
    /// `SVN_ERR_FS_NO_SUCH_LOCK`.
    pub const FS_NO_SUCH_LOCK: u32 = 160_040;
    /// `SVN_ERR_FS_LOCK_EXPIRED`.
    pub const FS_LOCK_EXPIRED: u32 = 160_041;
    /// `SVN_ERR_FS_OUT_OF_DATE`.
    pub const FS_OUT_OF_DATE: u32 = 160_042;
    /// `SVN_ERR_REPOS_HOOK_FAILURE`.
    pub const REPOS_HOOK_FAILURE: u32 = 165_001;
    /// `SVN_ERR_REPOS_DISABLED_FEATURE`.
    pub const REPOS_DISABLED_FEATURE: u32 = 165_006;
    /// `SVN_ERR_RA_NOT_AUTHORIZED`.
    pub const RA_NOT_AUTHORIZED: u32 = 170_001;
    /// `SVN_ERR_RA_NOT_IMPLEMENTED`.
    pub const RA_NOT_IMPLEMENTED: u32 = 170_003;
    /// `SVN_ERR_RA_OUT_OF_DATE`.
    pub const RA_OUT_OF_DATE: u32 = 170_004;
    /// `SVN_ERR_RA_NOT_LOCKED`.
    pub const RA_NOT_LOCKED: u32 = 170_007;
    /// `SVN_ERR_RA_DAV_PATH_NOT_FOUND`.
    pub const RA_DAV_PATH_NOT_FOUND: u32 = 175_007;
    /// `SVN_ERR_RA_DAV_CONN_TIMEOUT`.
    pub const RA_DAV_CONN_TIMEOUT: u32 = 175_012;
    /// `SVN_ERR_RA_DAV_FORBIDDEN`.
    pub const RA_DAV_FORBIDDEN: u32 = 175_013;
    /// `SVN_ERR_CLIENT_FORBIDDEN_BY_SERVER`.
    pub const CLIENT_FORBIDDEN_BY_SERVER: u32 = 195_023;
    /// `SVN_ERR_UNSUPPORTED_FEATURE`.
    pub const UNSUPPORTED_FEATURE: u32 = 200_007;
    /// `SVN_ERR_CANCELLED`.
    pub const CANCELLED: u32 = 200_015;
    /// `SVN_ERR_AUTHN_CREDS_UNAVAILABLE`.
    pub const AUTHN_CREDS_UNAVAILABLE: u32 = 215_000;
    /// `SVN_ERR_AUTHN_NO_PROVIDER`.
    pub const AUTHN_NO_PROVIDER: u32 = 215_001;
    /// `SVN_ERR_AUTHN_PROVIDERS_EXHAUSTED`.
    pub const AUTHN_PROVIDERS_EXHAUSTED: u32 = 215_002;
    /// `SVN_ERR_AUTHN_FAILED`.
    pub const AUTHN_FAILED: u32 = 215_004;
    /// `SVN_ERR_AUTHZ_ROOT_UNREADABLE`.
    pub const AUTHZ_ROOT_UNREADABLE: u32 = 220_000;
    /// `SVN_ERR_AUTHZ_UNREADABLE`.
    pub const AUTHZ_UNREADABLE: u32 = 220_001;
    /// `SVN_ERR_AUTHZ_PARTIALLY_READABLE`.
    pub const AUTHZ_PARTIALLY_READABLE: u32 = 220_002;
    /// `SVN_ERR_AUTHZ_UNWRITABLE`.
    pub const AUTHZ_UNWRITABLE: u32 = 220_004;
    /// `SVN_ERR_RA_SERF_SSL_CERT_UNTRUSTED`.
    pub const RA_SERF_SSL_CERT_UNTRUSTED: u32 = 230_001;
}

/// Maps a Subversion error code to the engine classification, `None` when it is not classified.
pub fn map_errcode(code: u32) -> Option<ErrorCode> {
    use codes::*;
    Some(match code {
        FS_NOT_FOUND | RA_DAV_PATH_NOT_FOUND | FS_NO_SUCH_REVISION => ErrorCode::PathNotFound,
        FS_OUT_OF_DATE | RA_OUT_OF_DATE | FS_CONFLICT | FS_TXN_OUT_OF_DATE => ErrorCode::OutOfDate,
        FS_PATH_ALREADY_LOCKED | FS_LOCK_OWNER_MISMATCH => ErrorCode::LockedByOther,
        FS_BAD_LOCK_TOKEN | FS_NO_LOCK_TOKEN | FS_NO_SUCH_LOCK | FS_LOCK_EXPIRED
        | FS_PATH_NOT_LOCKED | RA_NOT_LOCKED => ErrorCode::LockTokenInvalid,
        RA_NOT_AUTHORIZED
        | FS_NO_USER
        | AUTHN_CREDS_UNAVAILABLE
        | AUTHN_NO_PROVIDER
        | AUTHN_PROVIDERS_EXHAUSTED
        | AUTHN_FAILED => ErrorCode::AuthRequired,
        AUTHZ_ROOT_UNREADABLE
        | AUTHZ_UNREADABLE
        | AUTHZ_PARTIALLY_READABLE
        | AUTHZ_UNWRITABLE
        | RA_DAV_FORBIDDEN
        | CLIENT_FORBIDDEN_BY_SERVER
        | REPOS_HOOK_FAILURE => ErrorCode::Forbidden,
        RA_SERF_SSL_CERT_UNTRUSTED => ErrorCode::TlsUntrusted,
        RA_DAV_CONN_TIMEOUT => ErrorCode::Timeout,
        RA_NOT_IMPLEMENTED | UNSUPPORTED_FEATURE | REPOS_DISABLED_FEATURE => {
            ErrorCode::ServerUnsupported
        }
        CANCELLED => ErrorCode::Cancelled,
        _ => return None,
    })
}

/// Fallback classification from the HTTP status alone.
pub fn map_http_status(status: u16) -> ErrorCode {
    match status {
        401 | 407 => ErrorCode::AuthRequired,
        403 => ErrorCode::Forbidden,
        404 | 410 => ErrorCode::PathNotFound,
        408 | 504 => ErrorCode::Timeout,
        409 => ErrorCode::OutOfDate,
        412 => ErrorCode::LockTokenInvalid,
        423 => ErrorCode::LockedByOther,
        501 => ErrorCode::ServerUnsupported,
        300..=399 | 502 | 503 => ErrorCode::Network,
        _ => ErrorCode::Internal,
    }
}

/// One link of a `D:error` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavErrorEntry {
    /// `errcode` attribute, when present and numeric.
    pub errcode: Option<u32>,
    /// Trimmed message text.
    pub message: String,
}

/// Parses a `D:error` document into its chain (outermost first). `None` when the body is not
/// such a document or carries no message.
pub fn parse_dav_error(body: &[u8]) -> Option<Vec<DavErrorEntry>> {
    let body = &body[..body.len().min(MAX_ERROR_BODY)];
    let mut reader = XmlReader::new();
    let mut events = reader.feed(body).ok()?;
    events.extend(reader.finish().ok()?);
    let mut entries = Vec::new();
    let mut root_ok = false;
    let mut current: Option<DavErrorEntry> = None;
    for (i, ev) in events.into_iter().enumerate() {
        match ev {
            XmlEvent::Start(t) if i == 0 => root_ok = t.name.is(&Ns::Dav, "error"),
            XmlEvent::Start(t)
                if t.name.is(&Ns::Apache, "human-readable") && entries.len() < MAX_CHAIN =>
            {
                current = Some(DavErrorEntry {
                    errcode: t.attr("errcode").and_then(|v| v.trim().parse().ok()),
                    message: String::new(),
                });
            }
            XmlEvent::Text(t) => {
                if let Some(c) = current.as_mut() {
                    c.message.push_str(&t);
                }
            }
            XmlEvent::End(n) if n.is(&Ns::Apache, "human-readable") => {
                if let Some(mut c) = current.take() {
                    c.message = c.message.trim().to_owned();
                    entries.push(c);
                }
            }
            _ => {}
        }
    }
    (root_ok && !entries.is_empty()).then_some(entries)
}

/// Builds the error for a failed response. `body` should be the (bounded) response body.
pub fn error_from_response(status: u16, _headers: &Headers, body: &[u8]) -> SvnError {
    if let Some(entries) = parse_dav_error(body) {
        return chain_from_entries(status, &entries);
    }
    let reason = match status {
        401 => "authentication required",
        403 => "access forbidden",
        404 => "not found",
        409 => "conflict",
        423 => "locked",
        _ => "request failed",
    };
    SvnError::new(map_http_status(status), format!("HTTP {status}: {reason}"))
}

fn chain_from_entries(status: u16, entries: &[DavErrorEntry]) -> SvnError {
    let mut child: Option<SvnError> = None;
    for entry in entries.iter().rev() {
        let code = entry
            .errcode
            .and_then(map_errcode)
            .unwrap_or(ErrorCode::Internal);
        let mut err = SvnError::new(code, entry.message.clone());
        if let Some(n) = entry.errcode {
            err = err.with_svn_code(n);
        }
        if let Some(c) = child.take() {
            err = err.with_child(c);
        }
        child = Some(err);
    }
    // `entries` is never empty here; fall back defensively instead of unwrapping.
    let mut top =
        child.unwrap_or_else(|| SvnError::new(map_http_status(status), format!("HTTP {status}")));
    let best = entries
        .iter()
        .find_map(|e| e.errcode.and_then(map_errcode))
        .unwrap_or_else(|| map_http_status(status));
    top.code = best;
    top
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dav_error(chain: &[(u32, &str)]) -> String {
        let mut s = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns" xmlns:C="svn:"><C:error/>"#,
        );
        for (c, m) in chain {
            s.push_str(&format!(
                r#"<m:human-readable errcode="{c}">{m}</m:human-readable>"#
            ));
        }
        s.push_str("</D:error>");
        s
    }

    #[test]
    fn maps_errcode_and_keeps_chain() {
        let body = dav_error(&[(160_042, "File is out of date"), (160_024, "inner")]);
        let e = error_from_response(409, &Headers::new(), body.as_bytes());
        assert_eq!(e.code, ErrorCode::OutOfDate);
        assert_eq!(e.svn_code, Some(160_042));
        assert_eq!(e.child.as_ref().map(|c| c.svn_code), Some(Some(160_024)));
    }

    #[test]
    fn unknown_errcode_falls_back_to_status_code() {
        let body = dav_error(&[(999_999, "weird")]);
        let e = error_from_response(404, &Headers::new(), body.as_bytes());
        assert_eq!(e.code, ErrorCode::PathNotFound);
        assert_eq!(e.svn_code, Some(999_999));
    }

    #[test]
    fn html_body_uses_http_status() {
        let e = error_from_response(401, &Headers::new(), b"<html><body>nope</body></html>");
        assert_eq!(e.code, ErrorCode::AuthRequired);
        assert!(e.message.contains("401"));
    }

    #[test]
    fn wrong_root_or_garbage_is_not_a_dav_error() {
        assert!(parse_dav_error(b"<x><y/></x>").is_none());
        assert!(parse_dav_error(b"\xff\xfe").is_none());
        assert!(parse_dav_error(b"").is_none());
    }

    #[test]
    fn table_covers_spec_examples() {
        assert_eq!(map_errcode(160_042), Some(ErrorCode::OutOfDate));
        assert_eq!(map_errcode(160_035), Some(ErrorCode::LockedByOther));
        assert_eq!(map_errcode(160_013), Some(ErrorCode::PathNotFound));
    }
}
