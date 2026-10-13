//! Authentication prompting contract.

use async_trait::async_trait;
use secrecy::SecretString;
use svn_types::{OriginKey, Timestamp};

/// Authentication scheme offered by a server or chosen by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthMethod {
    /// Username and password (HTTPS only).
    Basic,
    /// SPNEGO / Kerberos single sign-on.
    Negotiate,
}

/// Credentials returned by the user.
#[derive(Debug)]
pub struct Credentials {
    /// User name.
    pub username: String,
    /// Password; redacted in `Debug` output.
    pub password: SecretString,
    /// Persist in the OS credential store.
    pub save: bool,
    /// Scheme these credentials are for.
    pub method: AuthMethod,
}

/// Why the TLS certificate needs a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertFailure {
    /// Issuer is not in the trust store.
    UnknownIssuer,
    /// Self-signed certificate.
    SelfSigned,
    /// Expired.
    Expired,
    /// Not valid yet.
    NotYetValid,
    /// Host name does not match.
    HostnameMismatch,
    /// Revoked.
    Revoked,
    /// Other verifier message.
    Other(String),
}

/// Certificate details shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// Server host.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Subject.
    pub subject: String,
    /// Issuer.
    pub issuer: String,
    /// Validity start.
    pub not_before: Timestamp,
    /// Validity end.
    pub not_after: Timestamp,
    /// SHA-256 fingerprint of the DER certificate.
    pub sha256: [u8; 32],
    /// Reason the platform verifier failed.
    pub failure: CertFailure,
}

/// User decision about an untrusted certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertDecision {
    /// Refuse the connection.
    Reject,
    /// Trust for this session only.
    AcceptOnce,
    /// Trust permanently; the transport only honors this for `UnknownIssuer` and `SelfSigned`.
    AcceptAlways,
}

/// Implemented by the app layer; called by the transport.
#[async_trait]
pub trait AuthPrompter: Send + Sync {
    /// Asks for credentials. The transport guarantees at most one concurrent call per
    /// `origin` (single-flight). `attempt` counts failed tries from 0. `None` means the user cancelled.
    async fn credentials(
        &self,
        origin: &OriginKey,
        attempt: u8,
        offered: &[AuthMethod],
    ) -> Option<Credentials>;

    /// Asks whether to trust a certificate that failed verification.
    async fn confirm_certificate(&self, info: &CertInfo) -> CertDecision;
}
