//! `AppError` (doc 03 section 4) and the mapping from engine error codes.

use serde::{Deserialize, Serialize};
use specta::Type;

use svn_types::{ErrorCode as SvnErrorCode, SvnError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    Network,
    Timeout,
    AuthRequired,
    Forbidden,
    TlsUntrusted,
    OutOfDate,
    LockedByOther,
    LockTokenInvalid,
    WcBusy,
    WcNeedsCleanup,
    WcFormatUnsupported,
    NotAWorkingCopy,
    ConflictBlocksCommit,
    PathNotFound,
    DiskFull,
    PermissionDenied,
    FileInUse,
    Cancelled,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorAction {
    Retry,
    Update,
    Cleanup,
    SignIn,
    TrustCert,
    OpenLockDialog,
    OpenSettings,
}

/// Error returned by every command. `message` is user-displayable; never a bare string error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    /// Technical detail: svn code (E160028), HTTP status, URL.
    pub detail: Option<String>,
    pub path: Option<String>,
    pub retryable: bool,
    pub actions: Option<Vec<ErrorAction>>,
}

impl AppError {
    /// Builds an error for `code` using the standard retryable flag and actions.
    pub fn from_code(code: SvnErrorCode, message: impl Into<String>) -> Self {
        let (ipc, retryable, actions) = map_svn_error_code(code);
        if code == SvnErrorCode::UnsafePath {
            tracing::warn!("unsafe path rejected by engine");
        }
        AppError {
            code: ipc,
            message: message.into(),
            detail: None,
            path: None,
            retryable,
            actions: (!actions.is_empty()).then(|| actions.to_vec()),
        }
    }

    /// Generic internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::from_code(SvnErrorCode::Internal, message)
    }

    /// Placeholder returned by command stubs that have no backend yet.
    pub fn not_implemented(command: &str) -> Self {
        let mut e = Self::internal("not implemented");
        e.detail = Some(command.to_owned());
        e
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

impl From<&SvnError> for AppError {
    /// Maps an engine error: `detail` carries the svn code (`E160028`) and the cause chain.
    fn from(err: &SvnError) -> Self {
        let mut out = AppError::from_code(err.code, err.message.clone());
        let mut parts = Vec::new();
        let mut cur = Some(err);
        while let Some(e) = cur {
            match e.svn_code {
                Some(code) => parts.push(format!("E{code:06}: {}", e.message)),
                None => parts.push(e.message.clone()),
            }
            cur = e.child.as_deref();
        }
        if parts.iter().any(|p| p.starts_with('E')) || parts.len() > 1 {
            out.detail = Some(parts.join(" <- "));
        }
        out
    }
}

/// Engine error code -> (IPC code, retryable, UI actions).
/// `Corrupt` maps to `wc-needs-cleanup`; `UnsafePath` and `ServerUnsupported` map to `internal`.
/// The match is exhaustive on purpose: a new engine code must be mapped explicitly.
pub fn map_svn_error_code(code: SvnErrorCode) -> (ErrorCode, bool, &'static [ErrorAction]) {
    use ErrorAction as A;
    use SvnErrorCode as S;
    match code {
        S::Network => (ErrorCode::Network, true, &[A::Retry]),
        S::Timeout => (ErrorCode::Timeout, true, &[A::Retry, A::OpenSettings]),
        S::AuthRequired => (ErrorCode::AuthRequired, false, &[A::SignIn]),
        S::Forbidden => (ErrorCode::Forbidden, false, &[]),
        S::TlsUntrusted => (ErrorCode::TlsUntrusted, false, &[A::TrustCert]),
        S::OutOfDate => (ErrorCode::OutOfDate, false, &[A::Update]),
        S::LockedByOther => (ErrorCode::LockedByOther, false, &[A::OpenLockDialog]),
        S::LockTokenInvalid => (ErrorCode::LockTokenInvalid, false, &[A::OpenLockDialog]),
        S::WcBusy => (ErrorCode::WcBusy, true, &[A::Retry]),
        S::WcNeedsCleanup => (ErrorCode::WcNeedsCleanup, false, &[A::Cleanup]),
        S::WcFormatUnsupported => (ErrorCode::WcFormatUnsupported, false, &[]),
        S::NotAWorkingCopy => (ErrorCode::NotAWorkingCopy, false, &[]),
        S::ConflictBlocksCommit => (ErrorCode::ConflictBlocksCommit, false, &[]),
        S::PathNotFound => (ErrorCode::PathNotFound, false, &[]),
        S::UnsafePath => (ErrorCode::Internal, false, &[]),
        S::DiskFull => (ErrorCode::DiskFull, false, &[]),
        S::PermissionDenied => (ErrorCode::PermissionDenied, false, &[]),
        S::FileInUse => (ErrorCode::FileInUse, true, &[A::Retry]),
        S::Cancelled => (ErrorCode::Cancelled, false, &[]),
        S::Corrupt => (ErrorCode::WcNeedsCleanup, false, &[A::Cleanup]),
        S::ServerUnsupported => (ErrorCode::Internal, false, &[]),
        S::Internal => (ErrorCode::Internal, false, &[]),
    }
}
