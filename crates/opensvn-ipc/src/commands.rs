//! Request structs for commands with many or optional parameters (docs/system-design/03 section 3).
//! Simple commands take plain arguments directly in `src-tauri`.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ids::{Rev, WcId};
use crate::models::WcFormat;
use crate::models::{
    Depth, DiffTarget, IgnoreScope, LockTargets, LogTarget, RemoteOp, ResolveChoice, SparseEntry,
};
use crate::path_grants::PathGrantId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StatusGetRequest {
    pub wc_id: WcId,
    pub include_ignored: Option<bool>,
    pub include_unversioned: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiffGetRequest {
    pub target: DiffTarget,
    pub ignore_whitespace: Option<bool>,
    pub force_large: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IgnoreAddRequest {
    pub wc_id: WcId,
    pub pattern: String,
    pub dir: String,
    pub scope: IgnoreScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutRequest {
    pub url: String,
    /// Folder chosen with `path_pick` (or minted by `checkout_root_grant`). With `dest_name` it is
    /// the parent folder and the working copy is created in a new child folder; without it the
    /// folder itself becomes the working copy root. An existing, non-empty target is refused.
    pub dest: PathGrantId,
    /// Name of the new folder created inside `dest`: one path component, no separators, not `.` or
    /// `..`, no reserved Windows name (`CON`, `NUL`, `COM1`, ...), no trailing dot or space.
    pub dest_name: Option<String>,
    pub rev: Option<Rev>,
    pub depth: Depth,
    pub sparse: Option<Vec<SparseEntry>>,
    pub format: WcFormat,
    pub store_pristine: bool,
    pub ignore_externals: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRequest {
    pub wc_id: WcId,
    pub paths: Option<Vec<String>>,
    pub rev: Option<Rev>,
    pub depth: Option<Depth>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CommitRequest {
    pub wc_id: WcId,
    pub paths: Vec<String>,
    pub message: String,
    pub keep_locks: bool,
    /// Always true in the MVP: unversioned/missing selected paths are added/removed automatically.
    pub auto_add_remove: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RevertRequest {
    pub wc_id: WcId,
    pub paths: Vec<String>,
    pub remove_unversioned: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CleanupRequest {
    pub wc_id: WcId,
    pub remove_unversioned: Option<bool>,
    pub remove_ignored: Option<bool>,
    pub vacuum_pristines: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ResolveRequest {
    pub wc_id: WcId,
    pub path: String,
    pub choice: ResolveChoice,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LogGetRequest {
    pub target: LogTarget,
    pub from_rev: Option<Rev>,
    pub to_rev: Option<Rev>,
    pub limit: u32,
    pub stop_on_copy: Option<bool>,
    pub with_paths: bool,
    pub filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LockRequest {
    pub targets: LockTargets,
    pub comment: Option<String>,
    pub steal: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UnlockRequest {
    pub targets: LockTargets,
    pub break_lock: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEditRequest {
    pub ops: Vec<RemoteOp>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CopyLocationRequest {
    pub from_url: String,
    pub from_rev: Option<Rev>,
    pub to_url: String,
    pub message: String,
    pub switch_wc_id: Option<WcId>,
    /// Copy the working copy (including its local changes) instead of `from_url`; `from_url` and
    /// `from_rev` are then ignored.
    pub from_wc: Option<WcId>,
}

/// Text that must never reach logs: `Debug` prints a fixed placeholder. On the wire it is a
/// plain JSON string. Convert to the engine's secret type as soon as it crosses into Rust.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(transparent)]
#[specta(transparent)]
pub struct SecretText(String);

impl SecretText {
    /// Returns the secret; callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretText {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretText(***)")
    }
}

/// Response of `auth_respond`'s input (answer to an `auth://prompt`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AuthResponse {
    Credentials {
        username: String,
        password: SecretText,
        save: bool,
    },
    Certificate {
        decision: CertDecision,
    },
    /// Use single sign-on (only valid when the prompt offered `negotiate`).
    Negotiate,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CertDecision {
    Reject,
    AcceptOnce,
    /// Trust permanently. Valid only for self-signed or unknown-issuer failures.
    AcceptAlways,
    /// Replace the pinned certificate of this host and port with the one now presented. Valid
    /// only when the failure is `changed`.
    ReplacePin,
}

/// Credential metadata only; never contains a password.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub realm: String,
    pub username: Option<String>,
    pub method: AccountMethod,
    pub last_used_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum AccountMethod {
    Basic,
    Negotiate,
}
