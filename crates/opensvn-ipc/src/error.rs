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
    /// Single sign-on (Negotiate/Kerberos) failed with no password fallback to offer: the server
    /// offers no Basic, the URL is plain http (SSO is https only), its mutual-authentication
    /// token was invalid, or the user chose SSO again after it had failed.
    NegotiateFailed,
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
    HookFailed,
    RepositoryMoved,
    PristineNotHydrated,
    WcInvalidOperation,
    PathNotFound,
    DiskFull,
    PermissionDenied,
    FileInUse,
    Cancelled,
    /// The request itself is invalid (bad value, expired path grant, unsupported option).
    InvalidArgument,
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
    /// The default app was refused for this item; show it in the file manager instead.
    RevealInFileManager,
    /// An interrupted checkout can be finished: see [`AppError::incomplete_checkout`].
    ContinueCheckout,
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
    /// For `locked-by-other`: the locks held by someone else on the requested paths, as the
    /// server reports them now. `None` when the engine could not ask (or for other codes).
    // A thin box keeps `AppError` (returned by every command) small.
    #[serde(default)]
    #[specta(optional)]
    #[allow(clippy::box_collection)]
    pub lock_conflicts: Option<Box<Vec<LockConflict>>>,
    /// A checkout found its target folder holding an interrupted checkout of the same location
    /// (or a checkout stopped and left one): the UI offers "Continue checkout".
    #[serde(default)]
    #[specta(optional)]
    pub incomplete_checkout: Option<Box<IncompleteCheckout>>,
}

/// An interrupted checkout the user can continue (`wc_continue`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IncompleteCheckout {
    pub wc_id: crate::WcId,
    /// Revision the checkout was reaching.
    pub rev: crate::Rev,
    pub root_path: String,
}

/// A lock held by someone else that refused a lock request (doc 01 §10.4: the lock dialog shows
/// the owner and offers Steal). Never carries the lock token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LockConflict {
    /// Requested path as the request named it (working copy relative path or URL).
    pub path: String,
    pub owner: String,
    pub comment: Option<String>,
    /// ISO 8601 creation time; `None` when the server sent none.
    pub created_at: Option<String>,
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
            lock_conflicts: None,
            incomplete_checkout: None,
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

    /// Attaches an interrupted checkout to continue, and offers the action.
    pub fn with_incomplete_checkout(mut self, info: IncompleteCheckout) -> Self {
        self.incomplete_checkout = Some(Box::new(info));
        let actions = self.actions.get_or_insert_with(Vec::new);
        if !actions.contains(&ErrorAction::ContinueCheckout) {
            actions.insert(0, ErrorAction::ContinueCheckout);
        }
        self
    }

    /// Attaches the locks that refused a lock request; an empty list attaches nothing.
    pub fn with_lock_conflicts(mut self, conflicts: Vec<LockConflict>) -> Self {
        self.lock_conflicts = (!conflicts.is_empty()).then(|| Box::new(conflicts));
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
/// `Corrupt` maps to `wc-needs-cleanup`; `UnsafePath` and `ServerUnsupported` map to `internal`;
/// `CommitOutcomeUnknown` and `WriteOutcomeUnknown` map to a non-retryable `network`; `LimitExceeded` maps to a
/// non-retryable `invalid-argument` with no actions.
/// The match is exhaustive on purpose: a new engine code must be mapped explicitly.
pub fn map_svn_error_code(code: SvnErrorCode) -> (ErrorCode, bool, &'static [ErrorAction]) {
    use ErrorAction as A;
    use SvnErrorCode as S;
    match code {
        S::Network => (ErrorCode::Network, true, &[A::Retry]),
        S::Timeout => (ErrorCode::Timeout, true, &[A::Retry, A::OpenSettings]),
        S::AuthRequired => (ErrorCode::AuthRequired, false, &[A::SignIn]),
        S::NegotiateFailed => (ErrorCode::NegotiateFailed, false, &[A::SignIn]),
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
        S::HookFailed => (ErrorCode::HookFailed, false, &[]),
        S::RepositoryMoved => (ErrorCode::RepositoryMoved, false, &[]),
        S::PristineNotHydrated => (ErrorCode::PristineNotHydrated, false, &[]),
        S::WcInvalidOperation => (ErrorCode::WcInvalidOperation, false, &[]),
        S::PathNotFound => (ErrorCode::PathNotFound, false, &[]),
        S::UnsafePath => (ErrorCode::Internal, false, &[]),
        S::DiskFull => (ErrorCode::DiskFull, false, &[]),
        S::PermissionDenied => (ErrorCode::PermissionDenied, false, &[]),
        S::FileInUse => (ErrorCode::FileInUse, true, &[A::Retry]),
        S::Cancelled => (ErrorCode::Cancelled, false, &[]),
        // Never retryable: re-sending a commit that may already be a revision could apply it
        // twice. The engine message says the outcome is unknown; reconciliation is the caller's.
        S::CommitOutcomeUnknown => (ErrorCode::Network, false, &[]),
        // Same for a lock or unlock that may have been carried out: refresh, then decide.
        S::WriteOutcomeUnknown => (ErrorCode::Network, false, &[]),
        // A size cap, not corruption: the same request fails again, and cleanup cannot help.
        S::LimitExceeded => (ErrorCode::InvalidArgument, false, &[]),
        S::Corrupt => (ErrorCode::WcNeedsCleanup, false, &[A::Cleanup]),
        S::ServerUnsupported => (ErrorCode::Internal, false, &[]),
        S::Internal => (ErrorCode::Internal, false, &[]),
    }
}
