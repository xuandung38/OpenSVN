//! Event payloads (Rust -> UI), docs/system-design/03 section 5.
//! Event names (`op://started`, ...) are bound in `src-tauri`.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::error::AppError;
use crate::ids::{OpId, Rev, WcId};
use crate::models::FileStatus;

/// Result payload of a finished operation; shape depends on the operation kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OpResult {
    /// No payload.
    Done,
    /// Commit or update produced/reached a revision.
    NewRev { new_rev: Rev },
    /// Checkout finished: the new working copy's registry id (`None` when it could not be
    /// registered; the files are on disk and can be added with "Add existing working copy").
    CheckedOut { wc_id: Option<WcId>, new_rev: Rev },
    /// Blame result.
    Blame {
        lines: Vec<crate::models::BlameLine>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OpFinished {
    pub op_id: OpId,
    pub result: OpResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OpFailed {
    pub op_id: OpId,
    pub error: AppError,
}

/// Status delta for a working copy: applied to snapshot `base` it yields snapshot `snapshot`
/// (see [`crate::models::StatusSnapshot`]). A holder of another snapshot than `base` fetches
/// the status again, unless it already holds `snapshot` or a later one. `base` is `None` when
/// the delta is against a snapshot nobody was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StatusChanged {
    pub wc_id: WcId,
    pub changed: Vec<FileStatus>,
    pub removed: Vec<String>,
    pub base: Option<u32>,
    pub snapshot: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RevisionChanged {
    pub wc_id: WcId,
    pub base_rev: Rev,
    pub rev_range: Option<(Rev, Rev)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Incoming {
    pub wc_id: WcId,
    pub count: u32,
    pub latest_rev: Rev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum BannerKind {
    Conflicts,
    MixedRev,
    LockStolen,
    NeedsCleanup,
    /// Another program (svn, TortoiseSVN, an IDE) holds the working copy lock. `data` lists the
    /// locked directories (`""` is the root).
    WcBusy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Banner {
    pub wc_id: WcId,
    pub kind: BannerKind,
    /// Banner-specific details (conflict paths, revision range, ...).
    pub data: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AuthPromptKind {
    Credentials,
    Certificate,
}

/// Why credentials are requested (mirrors the transport's prompt reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum AuthPromptReason {
    /// First request for this origin.
    Initial,
    /// The server rejected the credentials just offered.
    Rejected,
    /// Single sign-on failed; the user may fall back to a password.
    NegotiateFailed,
    /// Stored credentials are no longer valid.
    Expired,
}

/// Authentication scheme the server offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethodKind {
    Basic,
    /// Single sign-on (Windows SSO / Kerberos).
    Negotiate,
}

/// Why a TLS certificate needs a decision (`unknown-issuer`, ..., `{ changed: { oldSha256, newSha256 } }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum CertFailureInfo {
    UnknownIssuer,
    SelfSigned,
    Expired,
    NotYetValid,
    HostnameMismatch,
    Revoked,
    /// A different certificate than the pinned one; "always trust" is not offered.
    Changed {
        old_sha256: String,
        new_sha256: String,
    },
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CertSummary {
    pub host: String,
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    pub sha256: String,
    pub failure: CertFailureInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuthPrompt {
    pub request_id: String,
    pub kind: AuthPromptKind,
    pub realm: String,
    pub url: String,
    pub cert: Option<CertSummary>,
    /// Why credentials are requested; `None` for certificate prompts.
    pub reason: Option<AuthPromptReason>,
    /// Failed tries so far in this prompt sequence, starting at 0.
    pub attempt: u32,
    /// Schemes the server offers (credentials prompts; empty for certificate prompts).
    pub offered: Vec<AuthMethodKind>,
}
