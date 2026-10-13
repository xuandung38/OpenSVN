//! Negotiate (SPNEGO, RFC 4559) single sign-on with Kerberos.
//!
//! The transport talks to a [`NegotiateProvider`]; the platform backends live in submodules:
//!
//! - Windows: SSPI `Negotiate` package through `cross-krb5` (always built). A context that
//!   negotiates NTLM instead of Kerberos is rejected: NTLM is not part of the MVP.
//! - macOS/Linux: system GSSAPI with the SPNEGO mechanism through `libgssapi`, behind the
//!   `negotiate-gssapi` cargo feature (needs the gssapi headers and libclang to build).
//!
//! Every backend call may block (credential cache, KDC round trip), so the transport only calls
//! them inside `tokio::task::spawn_blocking`. Mutual authentication is requested; when the server
//! returns a final token with its response, the context must accept it or the response is
//! discarded with [`ErrorCode::NegotiateFailed`].

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderValue, WWW_AUTHENTICATE};
use svn_types::{CancelToken, ErrorCode, Result, SvnError};

#[cfg(all(unix, feature = "negotiate-gssapi"))]
mod gssapi;
#[cfg(windows)]
mod sspi;

/// Outcome of feeding a server token to a context.
#[derive(Debug, PartialEq, Eq)]
pub enum NegotiateStep {
    /// The context is established (mutual authentication verified by the backend).
    Complete,
    /// The mechanism wants another round trip with this token. HTTP Negotiate cannot keep a
    /// connection pinned across requests here, so the transport treats this as a failure.
    Continue(Vec<u8>),
}

/// A client security context for one HTTP request.
///
/// Methods may block and are only called from `spawn_blocking`.
pub trait NegotiateContext: Send {
    /// Consumes a token sent by the server.
    fn step(&mut self, token: &[u8]) -> Result<NegotiateStep>;
}

/// Creates Negotiate contexts for a host using the current user's Kerberos credentials.
pub trait NegotiateProvider: Send + Sync {
    /// Starts a context for the service `HTTP/<host>` and returns the first token to send.
    /// May block; only called from `spawn_blocking`.
    fn initiate(&self, host: &str) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)>;
}

/// The platform provider, or `None` when this build has no Negotiate support.
pub fn system_provider() -> Option<Arc<dyn NegotiateProvider>> {
    #[cfg(windows)]
    {
        Some(Arc::new(sspi::SspiProvider))
    }
    #[cfg(all(unix, feature = "negotiate-gssapi"))]
    {
        Some(Arc::new(gssapi::GssapiProvider))
    }
    #[cfg(not(any(windows, all(unix, feature = "negotiate-gssapi"))))]
    {
        None
    }
}

/// Builds an [`ErrorCode::NegotiateFailed`] error.
pub fn negotiate_failed(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::NegotiateFailed, msg)
}

/// A context waiting for the server's final (mutual authentication) token.
pub(crate) struct PendingNegotiate {
    ctx: Mutex<Option<Box<dyn NegotiateContext>>>,
}

impl std::fmt::Debug for PendingNegotiate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingNegotiate").finish_non_exhaustive()
    }
}

impl PendingNegotiate {
    fn take(&self) -> Option<Box<dyn NegotiateContext>> {
        self.ctx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Starts a context off the async runtime and returns the header plus the pending context.
pub(crate) async fn start(
    provider: &Arc<dyn NegotiateProvider>,
    host: &str,
    cancel: &CancelToken,
) -> Result<(HeaderValue, Arc<PendingNegotiate>)> {
    let provider = Arc::clone(provider);
    let host = host.to_owned();
    let job = tokio::task::spawn_blocking(move || provider.initiate(&host));
    let (ctx, token) = super::or_cancel(cancel, job).await?.map_err(join_error)??;
    let header = negotiate_header(&token)?;
    Ok((
        header,
        Arc::new(PendingNegotiate {
            ctx: Mutex::new(Some(ctx)),
        }),
    ))
}

/// Validates the server's final token, if it sent one, off the async runtime.
///
/// Returns `Ok(true)` when the token was verified, `Ok(false)` when the server sent none (mutual
/// authentication unavailable; the response is still accepted, as browsers and svn do).
pub(crate) async fn finish(
    pending: &PendingNegotiate,
    headers: &HeaderMap,
    cancel: &CancelToken,
) -> Result<bool> {
    let Some(token) = server_token(headers)? else {
        return Ok(false);
    };
    let Some(mut ctx) = pending.take() else {
        return Ok(false);
    };
    let job = tokio::task::spawn_blocking(move || ctx.step(&token));
    let step = super::or_cancel(cancel, job).await?.map_err(join_error)?;
    match step {
        Ok(NegotiateStep::Complete) => Ok(true),
        Ok(NegotiateStep::Continue(_)) => Err(negotiate_failed(
            "server did not complete Negotiate mutual authentication",
        )),
        Err(e) => {
            Err(negotiate_failed("server failed Negotiate mutual authentication").with_child(e))
        }
    }
}

/// `Authorization: Negotiate <base64>`, marked sensitive.
pub(crate) fn negotiate_header(token: &[u8]) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Negotiate {}", STANDARD.encode(token)))
        .map_err(|_| SvnError::new(ErrorCode::Internal, "Negotiate token is not a valid header"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The token in `WWW-Authenticate: Negotiate <base64>`, if the response carries one.
pub(crate) fn server_token(headers: &HeaderMap) -> Result<Option<Vec<u8>>> {
    for value in headers.get_all(WWW_AUTHENTICATE) {
        let Ok(text) = value.to_str() else { continue };
        for part in text.split(',') {
            let part = part.trim();
            let (scheme, rest) = part.split_once(char::is_whitespace).unwrap_or((part, ""));
            let rest = rest.trim();
            if !scheme.eq_ignore_ascii_case("negotiate") || rest.is_empty() {
                continue;
            }
            return STANDARD
                .decode(rest)
                .map(Some)
                .map_err(|_| negotiate_failed("server sent a malformed Negotiate token"));
        }
    }
    Ok(None)
}

fn join_error(e: tokio::task::JoinError) -> SvnError {
    negotiate_failed(format!("Negotiate worker failed: {e}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(WWW_AUTHENTICATE, HeaderValue::from_str(v).expect("header"));
        }
        h
    }

    #[test]
    fn header_is_sensitive_base64() {
        let v = negotiate_header(b"\x60\x01tok").expect("header");
        assert_eq!(v.to_str().expect("ascii"), "Negotiate YAF0b2s=");
        assert!(v.is_sensitive());
    }

    #[test]
    fn server_token_parsing() {
        assert_eq!(server_token(&headers(&["Negotiate"])).expect("ok"), None);
        assert_eq!(
            server_token(&headers(&[r#"Basic realm="r""#, "Negotiate YAF0b2s="])).expect("ok"),
            Some(b"\x60\x01tok".to_vec())
        );
        assert_eq!(
            server_token(&headers(&["negotiate  YAF0b2s=  "])).expect("ok"),
            Some(b"\x60\x01tok".to_vec())
        );
        let err = server_token(&headers(&["Negotiate !!!"])).expect_err("malformed");
        assert_eq!(err.code, ErrorCode::NegotiateFailed);
    }
}
