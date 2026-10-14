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
/// | every other failure (hostname mismatch, expired, not yet valid, revoked, other, and any failure added later) | yes | **no** | **no** | **no** |
///
/// Those other failures are hard errors with no override, not even `AcceptOnce`: OpenSVN is
/// intentionally stricter than the svn command line client. The only valid decision is `Reject`. For a changed
/// certificate the only valid choices are `Reject` and `ReplacePin` (replace the stored pin after
/// the user confirmed the new fingerprint). `ReplacePin` is valid only for `Changed`. The check
/// is an allow-list, so a failure added in the future is rejected until it is listed.
///
/// **The UI is not a trust boundary.** The transport must check the decision against the actual
/// failure with [`CertDecision::is_valid_for`] and treat an invalid combination as `Reject`,
/// whatever the app layer or the webview sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CertDecision {
    /// Refuse the connection. Always valid.
    Reject,
    /// Trust for this session only. Valid only for `UnknownIssuer` and `SelfSigned`.
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
        // Allow-list: anything not named here is invalid, including failures added later.
        matches!(
            (self, failure),
            (Self::Reject, _)
                | (Self::ReplacePin, CertFailure::Changed { .. })
                | (
                    Self::AcceptOnce | Self::AcceptAlways,
                    CertFailure::UnknownIssuer | CertFailure::SelfSigned
                )
        )
    }
}

/// Implemented by the app layer; called by the transport.
#[async_trait]
pub trait AuthPrompter: Send + Sync {
    /// Asks for credentials. The transport guarantees at most one concurrent call per
    /// `req.origin` (single-flight). `None` means the user cancelled.
    ///
    /// Do not persist the answer here even when `save` is set: the server has not seen it yet.
    /// Persist it in [`Self::credentials_accepted`].
    async fn credentials(&self, req: &CredentialRequest) -> Option<Credentials>;

    /// Credentials saved earlier for `req.origin` (for example in the OS credential store),
    /// without asking the user. The transport calls this before the first prompt for an
    /// origin, in interactive and in unattended mode, and uses the answer silently; if the
    /// server rejects it, an interactive request goes on to [`Self::credentials`] with
    /// [`PromptReason::Rejected`], an unattended one fails with `AuthRequired`.
    ///
    /// Must never show UI. The default has nothing stored.
    ///
    /// Stored credentials are offered at most once per origin **per transport** (an
    /// `HttpTransport` and every session sharing it). Background work (remote status polling,
    /// cleanup of abandoned transactions) must therefore reuse one long-lived transport: a
    /// fresh transport per poll would submit a stale stored password on every poll, and with
    /// a directory server behind the repository that counts towards the account lockout
    /// threshold without the user ever being asked. [`Self::stored_credentials_rejected`]
    /// lets the app stop offering a password the server refused, which also protects
    /// transports created later.
    async fn stored_credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        let _ = req;
        None
    }

    /// The server answered 401 to the credentials [`Self::stored_credentials`] returned for
    /// `req.origin`, in interactive or unattended mode. Called at most once per offer, before
    /// the transport prompts (interactive) or fails with `AuthRequired` (unattended); `req`
    /// carries [`PromptReason::Rejected`].
    ///
    /// The app should stop returning that password from [`Self::stored_credentials`] until the
    /// user enters a new one, so later transports do not submit it again. A 403 is not
    /// reported: mod_dav_svn answers 403 to valid credentials that lack access to a path,
    /// which says nothing about the password.
    ///
    /// **Mark the password stale; do not delete it.** A 401 does not always mean the password
    /// is wrong: on Apache 2.4 an authenticated user who fails a mod_authz_core `Require`
    /// rule (for example `Require ldap-group`) also gets 401, unless the server sets
    /// `AuthzSendForbiddenOnFailure On`. So the app should stop offering it automatically,
    /// keep it in the store, and re-prompt (pre-filling the user name) when the user next
    /// acts; a correct password then costs one prompt instead of being lost.
    ///
    /// Must never show UI and should return promptly (in-memory bookkeeping): the transport
    /// does not race this call against cancellation, so that a cancel cannot drop the report;
    /// a pending cancel takes effect as soon as it returns. The default does nothing.
    async fn stored_credentials_rejected(&self, req: &CredentialRequest) {
        let _ = req;
    }

    /// The server accepted credentials that [`Self::credentials`] returned for `origin`: the
    /// first response to a request carrying them was neither a 401 nor a server error. Called
    /// at most once per answer, whether or not `credentials.save` is set; never for
    /// credentials from [`Self::stored_credentials`].
    ///
    /// **Persist a password the user asked to save here, not in [`Self::credentials`].** A
    /// password saved before the server answered could be wrong; it would then be offered
    /// silently after every restart and count towards a directory server's lockout threshold.
    /// Credentials the server rejects never reach this hook.
    ///
    /// Must never show UI and should return promptly: the transport does not race it against
    /// cancellation, and the response is handed to the caller once it returns. The default
    /// does nothing.
    async fn credentials_accepted(&self, origin: &OriginKey, credentials: &Credentials) {
        let _ = (origin, credentials);
    }

    /// Asks whether to trust a certificate that failed verification.
    async fn confirm_certificate(&self, info: &CertInfo) -> CertDecision;
}
