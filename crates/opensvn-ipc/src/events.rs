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

/// Status delta for a working copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StatusChanged {
    pub wc_id: WcId,
    pub changed: Vec<FileStatus>,
    pub removed: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CertSummary {
    pub host: String,
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
    pub sha256: String,
    pub failure: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuthPrompt {
    pub request_id: String,
    pub kind: AuthPromptKind,
    pub realm: String,
    pub url: String,
    pub cert: Option<CertSummary>,
}
