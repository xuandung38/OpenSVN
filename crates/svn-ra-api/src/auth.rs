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

/// Why the credentials are being requested; lets the UI word the prompt without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PromptReason {
    /// First request for this origin: nothing usable is stored.
    Initial,
    /// The server rejected the credentials that were just offered (stored or typed).
    Rejected,
    /// Negotiate (SSO) was attempted and failed; the user may fall back to Basic.
    NegotiateFailed,
}

/// Everything the transport tells the app when it needs credentials.
///
/// Marked `non_exhaustive` so fields can be added without breaking implementors; the transport
/// builds it with [`CredentialRequest::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CredentialRequest {
    /// Credential scope being asked for.
    pub origin: OriginKey,
    /// Failed tries so far for this prompt sequence, starting at 0.
    pub attempt: u32,
    /// Schemes the server offers, in server preference order.
    pub offered: Vec<AuthMethod>,
    /// Why the prompt is shown.
    pub reason: PromptReason,
}

impl CredentialRequest {
    /// Creates a request.
    pub fn new(
        origin: OriginKey,
        attempt: u32,
        offered: Vec<AuthMethod>,
        reason: PromptReason,
    ) -> Self {
        Self {
            origin,
            attempt,
            offered,
            reason,
        }
    }
}

/// Why the TLS certificate needs a decision.
///
/// When several problems apply to one certificate, report the most severe one: hostname
/// mismatch, expired, revoked and not-yet-valid outrank `Changed`, which outranks
/// `SelfSigned` and `UnknownIssuer`. A certificate that is both changed and hostname-mismatched
/// is reported as `HostnameMismatch`, never as `Changed`, so the user is not offered a way to
/// trust it. Only the transport classifies; this type just carries the result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The server presented a different certificate than the one pinned for this origin
    /// (for example after a renewal). Only `Reject` and `ReplacePin` are valid decisions.
    Changed {
        /// SHA-256 fingerprint of the pinned certificate.
        old_sha256: [u8; 32],
        /// SHA-256 fingerprint of the certificate now presented.
        new_sha256: [u8; 32],
    },
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
///
/// Which decisions are valid depends on the failure being decided:
///
/// | failure | `Reject` | `AcceptOnce` | `AcceptAlways` | `ReplacePin` |
/// |---|---|---|---|---|
/// | `UnknownIssuer`, `SelfSigned` | yes | yes | yes | no |
/// | `Changed` | yes | **no** | **no** | yes |
/// | every other failure | yes | yes | no | no |
///
/// For a changed certificate the only valid choices are `Reject` and `ReplacePin` (replace the
/// stored pin after the user confirmed the new fingerprint). `ReplacePin` is valid only for
/// `Changed`.
///
/// **The UI is not a trust boundary.** The transport must check the decision against the actual
/// failure with [`CertDecision::is_valid_for`] and treat an invalid combination as `Reject`,
/// whatever the app layer or the webview sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CertDecision {
    /// Refuse the connection. Always valid.
    Reject,
    /// Trust for this session only. Not valid for `Changed`.
    AcceptOnce,
    /// Trust permanently. Valid only for `UnknownIssuer` and `SelfSigned`.
    AcceptAlways,
    /// Replace the pinned certificate with the one now presented. Valid only for `Changed`.
    ReplacePin,
}

impl CertDecision {
    /// Whether this decision is allowed for `failure` (see the table on [`CertDecision`]).
    /// The transport calls this on every decision it receives and rejects invalid ones.
    pub fn is_valid_for(self, failure: &CertFailure) -> bool {
        match (self, failure) {
            (Self::Reject, _) => true,
            (Self::ReplacePin, CertFailure::Changed { .. }) => true,
            (_, CertFailure::Changed { .. }) | (Self::ReplacePin, _) => false,
            (Self::AcceptOnce, _) => true,
            (Self::AcceptAlways, CertFailure::UnknownIssuer | CertFailure::SelfSigned) => true,
            (Self::AcceptAlways, _) => false,
        }
    }
}

/// Implemented by the app layer; called by the transport.
#[async_trait]
pub trait AuthPrompter: Send + Sync {
    /// Asks for credentials. The transport guarantees at most one concurrent call per
    /// `req.origin` (single-flight). `None` means the user cancelled.
    async fn credentials(&self, req: &CredentialRequest) -> Option<Credentials>;

    /// Asks whether to trust a certificate that failed verification.
    async fn confirm_certificate(&self, info: &CertInfo) -> CertDecision;
}
