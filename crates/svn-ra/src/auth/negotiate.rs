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
//!
//! Security rules enforced around the backends:
//!
//! - Tokens are only sent over `https` (the transport refuses Negotiate on plain `http`).
//! - The service name is pinned to the request URL host ([`service_host`]): a trailing dot is
//!   stripped and IP literals are refused, never derived from anything the server sends.
//! - Kerberos only: both backends inspect their first token with [`check_kerberos_token`] before
//!   it is sent, so a context that would start NTLM (or any other mechanism) never leaves the
//!   machine. This holds whether or not the server later returns a final token. When it does, the
//!   completed context is checked again (SSPI: by `cross-krb5`; GSSAPI: the negotiated mech OID).
//! - TLS channel bindings (`tls-server-end-point`, RFC 5929) are passed to the backend when the
//!   transport knows the server certificate, for servers with Extended Protection enabled.

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
    ///
    /// `host` is the request URL host as returned by [`service_host`] (lowercase, no trailing
    /// dot, never an IP literal). `channel_bindings`, when present, is the RFC 5929
    /// `tls-server-end-point:<hash>` application data for the server certificate.
    /// May block; only called from `spawn_blocking`.
    fn initiate(
        &self,
        host: &str,
        channel_bindings: Option<&[u8]>,
    ) -> Result<(Box<dyn NegotiateContext>, Vec<u8>)>;
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

/// The host name to build the Kerberos service name from: the URL host with a trailing dot
/// removed. IP literals are refused because no `HTTP/<ip>` service principal exists and SSPI
/// would silently fall back to NTLM.
pub fn service_host(host: &str) -> Result<String> {
    let name = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let is_ip =
        name.starts_with('[') || name.contains(':') || name.parse::<std::net::Ipv4Addr>().is_ok();
    if is_ip {
        return Err(negotiate_failed(format!(
            "single sign-on needs the server's full host name (for example svn.example.com), \
             not the IP address {name}; change the repository URL to use the host name"
        )));
    }
    if name.is_empty() {
        return Err(negotiate_failed(
            "request URL has no host name for single sign-on",
        ));
    }
    Ok(name)
}

// DER-encoded OID bodies.
const OID_SPNEGO: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];
const OID_KRB5: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
/// The legacy Microsoft Kerberos OID (1.2.840.48018.1.2.2) that Windows lists first.
const OID_KRB5_MS: &[u8] = &[0x2a, 0x86, 0x48, 0x82, 0xf7, 0x12, 0x01, 0x02, 0x02];
const OID_NTLM: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a];
const OID_NEGOEX: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x1e];

/// One DER TLV: `(tag, contents, rest)`. Definite lengths only.
fn der(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        (len, &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// The mechanism an initial context token is for: the token's own OID for a raw mechanism
/// token, or the first (optimistic) entry of `mechTypes` for a SPNEGO `NegTokenInit`.
fn initial_token_mech(token: &[u8]) -> Option<&[u8]> {
    let (0x60, body, _) = der(token)? else {
        return None;
    };
    let (0x06, oid, inner) = der(body)? else {
        return None;
    };
    if oid != OID_SPNEGO {
        return Some(oid);
    }
    // [0] NegTokenInit ::= SEQUENCE { mechTypes [0] SEQUENCE OF OID, ... }
    let (0xa0, init, _) = der(inner)? else {
        return None;
    };
    let (0x30, fields, _) = der(init)? else {
        return None;
    };
    let (0xa0, mech_types, _) = der(fields)? else {
        return None;
    };
    let (0x30, list, _) = der(mech_types)? else {
        return None;
    };
    let (0x06, first, _) = der(list)? else {
        return None;
    };
    Some(first)
}

/// Refuses an initial token that does not start Kerberos. Backends call this before returning
/// a token, so an NTLM (or NegoEx, or unknown) exchange is never started.
pub fn check_kerberos_token(token: &[u8]) -> Result<()> {
    match initial_token_mech(token) {
        Some(oid) if oid == OID_KRB5 || oid == OID_KRB5_MS => Ok(()),
        Some(oid) => {
            let what = match oid {
                OID_NTLM => "NTLM",
                OID_NEGOEX => "NegoEx",
                _ => "a mechanism other than Kerberos",
            };
            Err(negotiate_failed(format!(
                "Kerberos is not available for this server (the system offered {what}, which \
                 OpenSVN does not use); check that you have a Kerberos ticket and that the \
                 repository URL uses the server's full host name"
            )))
        }
        None => Err(negotiate_failed(
            "the system produced an unrecognised Negotiate token",
        )),
    }
}

/// Whether a mechanism OID (DER body) is Kerberos 5.
#[cfg_attr(not(all(unix, feature = "negotiate-gssapi")), allow(dead_code))]
pub(crate) fn is_kerberos_oid(oid: &[u8]) -> bool {
    oid == OID_KRB5 || oid == OID_KRB5_MS
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
    channel_bindings: Option<Arc<[u8]>>,
    cancel: &CancelToken,
) -> Result<(HeaderValue, Arc<PendingNegotiate>)> {
    let provider = Arc::clone(provider);
    let host = service_host(host)?;
    let job =
        tokio::task::spawn_blocking(move || provider.initiate(&host, channel_bindings.as_deref()));
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

    #[test]
    fn service_host_is_pinned_to_the_url_host() {
        assert_eq!(
            service_host("SVN.Example.com.").expect("ok"),
            "svn.example.com"
        );
        assert_eq!(service_host("svn").expect("short"), "svn");
        for ip in ["10.0.0.1", "[::1]", "::1", "[fe80::1]"] {
            let e = service_host(ip).expect_err(ip);
            assert_eq!(e.code, ErrorCode::NegotiateFailed);
            assert!(e.message.contains("full host name"), "{}", e.message);
        }
    }

    fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        if body.len() < 0x80 {
            v.push(u8::try_from(body.len()).expect("len"));
        } else {
            let len = u16::try_from(body.len()).expect("len").to_be_bytes();
            v.extend_from_slice(&[0x82, len[0], len[1]]);
        }
        v.extend_from_slice(body);
        v
    }

    fn spnego(mechs: &[&[u8]], mech_token: &[u8]) -> Vec<u8> {
        let list: Vec<u8> = mechs.iter().flat_map(|m| tlv(0x06, m)).collect();
        let mut fields = tlv(0xa0, &tlv(0x30, &list));
        fields.extend(tlv(0xa2, &tlv(0x04, mech_token)));
        let mut body = tlv(0x06, OID_SPNEGO);
        body.extend(tlv(0xa0, &tlv(0x30, &fields)));
        tlv(0x60, &body)
    }

    fn raw(oid: &[u8]) -> Vec<u8> {
        let mut body = tlv(0x06, oid);
        body.extend_from_slice(&[0x01, 0x00, 0x6e, 0x00]);
        tlv(0x60, &body)
    }

    #[test]
    fn only_kerberos_initial_tokens_pass() {
        let ap_req = raw(OID_KRB5);
        check_kerberos_token(&ap_req).expect("raw krb5");
        // Windows: MS-KRB5 first, long mechToken (multi-byte DER length).
        let big = vec![0xab; 1500];
        check_kerberos_token(&spnego(&[OID_KRB5_MS, OID_KRB5, OID_NTLM], &big)).expect("ms");
        check_kerberos_token(&spnego(&[OID_KRB5, OID_NTLM], &ap_req)).expect("mit");

        let e = check_kerberos_token(&spnego(&[OID_NTLM], b"NTLMSSP\0")).expect_err("ntlm");
        assert_eq!(e.code, ErrorCode::NegotiateFailed);
        assert!(e.message.contains("NTLM"), "{}", e.message);
        let e = check_kerberos_token(&spnego(&[OID_NEGOEX, OID_KRB5], b"x")).expect_err("negoex");
        assert!(e.message.contains("NegoEx"), "{}", e.message);
        check_kerberos_token(&raw(OID_NTLM)).expect_err("raw ntlm");
        for junk in [
            &b""[..],
            b"\x60",
            b"\x60\x05\x06\x01",
            b"client-ap-req",
            &[0x60, 0x84, 0xff, 0xff, 0xff, 0xff],
        ] {
            check_kerberos_token(junk).expect_err("junk");
        }
    }
}
