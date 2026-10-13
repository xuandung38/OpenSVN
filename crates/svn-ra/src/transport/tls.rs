//! TLS verification policy on top of the platform verifier.
//!
//! The platform verifier (Windows CryptoAPI, macOS Security.framework, system CA bundle on
//! Linux) decides first. When it rejects a certificate this module classifies the failure:
//!
//! * `UnknownIssuer` / `SelfSigned` can be overridden by the user, either for the session or
//!   permanently by pinning `(host, port, SHA-256 of the leaf)` in the [`TrustStore`].
//! * `HostnameMismatch`, `Revoked`, `Expired`, `NotYetValid` and anything unrecognized are
//!   hard failures: they are never offered to the user and a pin does not override them.
//!
//! The rustls verifier callback is synchronous, so it only records the problem; the transport
//! reads it after the failed connect and asks the `AuthPrompter` asynchronously.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use svn_ra_api::{CertFailure, CertInfo};
use svn_types::{ErrorCode, SvnError, Timestamp};

/// Trusted certificate fingerprints supplied by the application (persisted in its settings).
///
/// Methods are called from inside the TLS handshake and must not block for long.
pub trait TrustStore: Send + Sync {
    /// Fingerprints pinned for this `(host, port)`.
    fn pinned(&self, host: &str, port: u16) -> Vec<[u8; 32]>;
    /// Pins a fingerprint after the user chose "accept always".
    fn pin(&self, host: &str, port: u16, sha256: [u8; 32]);
    /// Replaces every pin of this `(host, port)` with `sha256` after the user accepted a
    /// changed certificate.
    fn replace_pins(&self, host: &str, port: u16, sha256: [u8; 32]);
}

type PinMap = HashMap<(String, u16), Vec<[u8; 32]>>;

/// Non-persistent [`TrustStore`] for tests and headless use.
#[derive(Debug, Default)]
pub struct MemoryTrustStore {
    pins: Mutex<PinMap>,
}

impl MemoryTrustStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TrustStore for MemoryTrustStore {
    fn pinned(&self, host: &str, port: u16) -> Vec<[u8; 32]> {
        let pins = self.pins.lock().unwrap_or_else(PoisonError::into_inner);
        pins.get(&(host.to_owned(), port))
            .cloned()
            .unwrap_or_default()
    }

    fn pin(&self, host: &str, port: u16, sha256: [u8; 32]) {
        let mut pins = self.pins.lock().unwrap_or_else(PoisonError::into_inner);
        let list = pins.entry((host.to_owned(), port)).or_default();
        if !list.contains(&sha256) {
            list.push(sha256);
        }
    }

    fn replace_pins(&self, host: &str, port: u16, sha256: [u8; 32]) {
        let mut pins = self.pins.lock().unwrap_or_else(PoisonError::into_inner);
        pins.insert((host.to_owned(), port), vec![sha256]);
    }
}

/// SHA-256 of a DER certificate.
pub fn sha256_fingerprint(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// A rejected certificate waiting for the transport to act on it.
#[derive(Debug, Clone)]
pub(crate) struct CertProblem {
    pub(crate) info: CertInfo,
    /// The user may accept it (`UnknownIssuer` / `SelfSigned`).
    pub(crate) overridable: bool,
    /// The host has pinned certificates and this one is different.
    pub(crate) changed: bool,
    /// Fingerprints that were pinned before the certificate changed.
    pub(crate) previous: Vec<[u8; 32]>,
}

impl CertProblem {
    /// Error for a problem that is not (or no longer) overridable.
    pub(crate) fn to_error(&self) -> SvnError {
        let target = format!("{}:{}", self.info.host, self.info.port);
        let msg = if self.changed {
            format!("the certificate of {target} changed since it was trusted")
        } else {
            match &self.info.failure {
                CertFailure::HostnameMismatch => {
                    format!("certificate of {target} does not match the host name")
                }
                CertFailure::Revoked => format!("certificate of {target} was revoked"),
                CertFailure::Expired => format!("certificate of {target} has expired"),
                CertFailure::NotYetValid => format!("certificate of {target} is not valid yet"),
                CertFailure::UnknownIssuer => {
                    format!("certificate of {target} is signed by an unknown issuer")
                }
                CertFailure::SelfSigned => format!("certificate of {target} is self-signed"),
                CertFailure::Other(m) => format!("certificate of {target} rejected: {m}"),
            }
        };
        SvnError::new(ErrorCode::TlsUntrusted, msg)
    }
}

/// What to persist when the user accepts a certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinAction {
    /// Trust for this session only.
    SessionOnly,
    /// Add to the host's pins.
    Add,
    /// The certificate changed: the new pin replaces the old ones.
    Replace,
}

/// Per-`(host, port)` TLS state shared between the verifier and the transport.
pub(crate) struct OriginTls {
    host: String,
    port: u16,
    trust: Arc<dyn TrustStore>,
    accepted_for_session: Mutex<HashSet<[u8; 32]>>,
    problem: Mutex<Option<(Instant, CertProblem)>>,
    /// Fingerprints the user refused since the last successful connection.
    rejected: Mutex<HashSet<[u8; 32]>>,
    /// Serializes certificate prompts so concurrent requests ask only once.
    pub(crate) prompt_gate: tokio::sync::Mutex<()>,
}

impl OriginTls {
    pub(crate) fn new(host: String, port: u16, trust: Arc<dyn TrustStore>) -> Self {
        Self {
            host,
            port,
            trust,
            accepted_for_session: Mutex::default(),
            problem: Mutex::default(),
            rejected: Mutex::default(),
            prompt_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Problem recorded at or after `since`. Older entries belong to earlier attempts and must
    /// not be blamed for an unrelated network failure.
    pub(crate) fn problem_since(&self, since: Instant) -> Option<CertProblem> {
        self.problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(at, _)| *at >= since)
            .map(|(_, p)| p.clone())
    }

    /// Whether this fingerprint is currently trusted (session accept or pin).
    pub(crate) fn is_trusted(&self, fp: &[u8; 32]) -> bool {
        self.accepted_for_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(fp)
            || self.trust.pinned(&self.host, self.port).contains(fp)
    }

    /// Trust `fp` for the rest of the session and apply the pin action.
    pub(crate) fn accept(&self, fp: [u8; 32], pin: PinAction) {
        self.accepted_for_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fp);
        match pin {
            PinAction::SessionOnly => {}
            PinAction::Add => self.trust.pin(&self.host, self.port, fp),
            PinAction::Replace => self.trust.replace_pins(&self.host, self.port, fp),
        }
        *self.problem.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Remembers a refusal so queued requests do not prompt again.
    pub(crate) fn reject(&self, fp: [u8; 32]) {
        self.rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(fp);
        *self.problem.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    pub(crate) fn was_rejected(&self, fp: &[u8; 32]) -> bool {
        self.rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(fp)
    }

    /// Called after a successful response: forget stale problems and refusals.
    pub(crate) fn connected(&self) {
        self.rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        *self.problem.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn record(&self, problem: CertProblem) {
        *self.problem.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), problem));
    }
}

/// Verifier that defers to the platform verifier and applies the override rules.
#[derive(Debug)]
struct PolicyVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    origin: Arc<OriginTls>,
}

impl std::fmt::Debug for OriginTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginTls")
            .field("host", &self.host)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Fields of the leaf certificate needed for classification and display.
struct LeafDetails {
    subject: String,
    issuer: String,
    self_issued: bool,
    not_before: i64,
    not_after: i64,
    dns_names: Vec<String>,
    ip_addrs: Vec<std::net::IpAddr>,
}

fn parse_leaf(der: &[u8]) -> Option<LeafDetails> {
    use x509_parser::extensions::GeneralName;
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let mut dns_names = Vec::new();
    let mut ip_addrs = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(d) => dns_names.push(d.to_ascii_lowercase()),
                GeneralName::IPAddress(b) => {
                    if let Ok(a) = <[u8; 4]>::try_from(*b) {
                        ip_addrs.push(std::net::IpAddr::from(a));
                    } else if let Ok(a) = <[u8; 16]>::try_from(*b) {
                        ip_addrs.push(std::net::IpAddr::from(a));
                    }
                }
                _ => {}
            }
        }
    }
    Some(LeafDetails {
        dns_names,
        ip_addrs,
        subject: cert.subject().to_string(),
        issuer: cert.issuer().to_string(),
        self_issued: cert.subject().as_raw() == cert.issuer().as_raw(),
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
    })
}

impl LeafDetails {
    /// Whether the certificate's subject alternative names cover `name`.
    fn covers(&self, name: &ServerName<'_>) -> bool {
        match name {
            ServerName::IpAddress(ip) => self.ip_addrs.contains(&std::net::IpAddr::from(*ip)),
            other => {
                let host = other.to_str().to_ascii_lowercase();
                let host = host.trim_end_matches('.');
                self.dns_names
                    .iter()
                    .any(|pattern| match pattern.strip_prefix("*.") {
                        // A wildcard covers exactly one left-most label.
                        Some(suffix) => host
                            .split_once('.')
                            .is_some_and(|(label, rest)| !label.is_empty() && rest == suffix),
                        None => pattern == host,
                    })
            }
        }
    }
}

/// `errSecNotTrusted`: Security.framework's code for a chain that ends in an untrusted anchor.
const ERR_SEC_NOT_TRUSTED: i32 = -67843;

/// Whether a platform-verifier message ends in the OSStatus for an untrusted anchor. The text
/// before the code is a localized description and is deliberately ignored.
fn is_untrusted_anchor(message: &str) -> bool {
    message
        .rsplit_once(':')
        .and_then(|(_, code)| code.trim().parse::<i32>().ok())
        == Some(ERR_SEC_NOT_TRUSTED)
}

fn classify(err: &TlsError, leaf: Option<&LeafDetails>, now_secs: i64) -> CertFailure {
    // Some platform verifiers report expiry only as an opaque message, so check the dates too.
    if let Some(l) = leaf {
        if now_secs > l.not_after {
            return CertFailure::Expired;
        }
        if now_secs < l.not_before {
            return CertFailure::NotYetValid;
        }
    }
    match err {
        TlsError::InvalidCertificate(c) => match c {
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                CertFailure::HostnameMismatch
            }
            CertificateError::Revoked => CertFailure::Revoked,
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                CertFailure::Expired
            }
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                CertFailure::NotYetValid
            }
            CertificateError::UnknownIssuer => {
                if leaf.is_some_and(|l| l.self_issued) {
                    CertFailure::SelfSigned
                } else {
                    CertFailure::UnknownIssuer
                }
            }
            // Platform verifiers without a dedicated code (macOS: "certificate is not trusted")
            // report an untrusted anchor this way.
            CertificateError::Other(o) if is_untrusted_anchor(&o.to_string()) => {
                if leaf.is_some_and(|l| l.self_issued) {
                    CertFailure::SelfSigned
                } else {
                    CertFailure::UnknownIssuer
                }
            }
            other => CertFailure::Other(format!("{other:?}")),
        },
        other => CertFailure::Other(other.to_string()),
    }
}

impl ServerCertVerifier for PolicyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let err = match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(ok) => return Ok(ok),
            Err(e) => e,
        };
        let fp = sha256_fingerprint(end_entity.as_ref());
        let now_secs = i64::try_from(now.as_secs()).unwrap_or(i64::MAX);
        let leaf = parse_leaf(end_entity.as_ref());
        let mut failure = classify(&err, leaf.as_ref(), now_secs);
        // Never let an override cover a certificate issued for a different host.
        if matches!(
            failure,
            CertFailure::UnknownIssuer | CertFailure::SelfSigned
        ) && !leaf.as_ref().is_some_and(|l| l.covers(server_name))
        {
            failure = CertFailure::HostnameMismatch;
        }
        let overridable = matches!(
            failure,
            CertFailure::UnknownIssuer | CertFailure::SelfSigned
        );
        let mut previous = Vec::new();
        if overridable {
            if self.origin.is_trusted(&fp) {
                return Ok(ServerCertVerified::assertion());
            }
            previous = self
                .origin
                .trust
                .pinned(&self.origin.host, self.origin.port);
        }
        let changed = !previous.is_empty();
        let micros = |s: i64| Timestamp(s.saturating_mul(1_000_000));
        self.origin.record(CertProblem {
            info: CertInfo {
                host: self.origin.host.clone(),
                port: self.origin.port,
                subject: leaf.as_ref().map(|l| l.subject.clone()).unwrap_or_default(),
                issuer: leaf.as_ref().map(|l| l.issuer.clone()).unwrap_or_default(),
                not_before: micros(leaf.as_ref().map_or(0, |l| l.not_before)),
                not_after: micros(leaf.as_ref().map_or(0, |l| l.not_after)),
                sha256: fp,
                failure,
            },
            overridable,
            changed,
            previous,
        });
        Err(err)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Creates the rustls client configuration for one `(host, port)`: HTTP/1.1 only, platform
/// verifier plus the override policy.
pub(crate) fn make_client_config(
    origin: Arc<OriginTls>,
    extra_roots: &[CertificateDer<'static>],
) -> Result<rustls::ClientConfig, SvnError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let platform = rustls_platform_verifier::Verifier::new_with_extra_roots(
        extra_roots.iter().cloned(),
        Arc::clone(&provider),
    )
    .map_err(|e| {
        SvnError::new(
            ErrorCode::Internal,
            format!("TLS verifier setup failed: {e}"),
        )
    })?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("TLS setup failed: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PolicyVerifier {
            inner: Arc::new(platform),
            origin,
        }))
        .with_no_client_auth();
    // Negotiate/NTLM bind to a connection, so never upgrade to HTTP/2.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use rustls::OtherError;

    use super::*;

    fn other(msg: &str) -> TlsError {
        #[derive(Debug)]
        struct Msg(String);
        impl std::fmt::Display for Msg {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Msg {}
        TlsError::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(Msg(
            msg.to_owned()
        )))))
    }

    fn leaf(self_issued: bool) -> LeafDetails {
        LeafDetails {
            subject: String::new(),
            issuer: String::new(),
            self_issued,
            not_before: 0,
            not_after: i64::MAX,
            dns_names: Vec::new(),
            ip_addrs: Vec::new(),
        }
    }

    #[test]
    fn untrusted_anchor_is_recognized_by_status_code_not_by_text() {
        for msg in [
            "\u{201c}x\u{201d} certificate is not trusted: -67843",
            "\u{ab}x\u{bb} n'est pas fiable : -67843",
            "\u{300c}x\u{300d}\u{306f}\u{4fe1}\u{983c}\u{3067}\u{304d}\u{307e}\u{305b}\u{3093}: -67843",
        ] {
            assert_eq!(
                classify(&other(msg), Some(&leaf(true)), 1),
                CertFailure::SelfSigned,
                "{msg}"
            );
            assert_eq!(
                classify(&other(msg), Some(&leaf(false)), 1),
                CertFailure::UnknownIssuer,
                "{msg}"
            );
        }
    }

    #[test]
    fn other_status_codes_and_plain_text_stay_hard_failures() {
        for msg in [
            "certificate is not trusted: -67828",
            "certificate is not trusted",
            "not trusted: abc",
            "-67843 is mentioned but not at the end: yes",
        ] {
            assert!(
                matches!(
                    classify(&other(msg), Some(&leaf(true)), 1),
                    CertFailure::Other(_)
                ),
                "{msg}"
            );
        }
    }
}
