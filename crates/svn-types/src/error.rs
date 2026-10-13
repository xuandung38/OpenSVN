//! Error model shared by every engine crate.

use std::fmt;

/// Coarse error classification; the app layer maps it to user-facing codes and actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Connection or transport failure.
    Network,
    /// Operation timed out.
    Timeout,
    /// Credentials are required or were rejected.
    AuthRequired,
    /// Server refused the operation (authorization).
    Forbidden,
    /// TLS certificate is not trusted.
    TlsUntrusted,
    /// Item or revision is out of date.
    OutOfDate,
    /// Path is locked by another user.
    LockedByOther,
    /// Supplied lock token is not valid.
    LockTokenInvalid,
    /// Working copy is locked by another operation.
    WcBusy,
    /// Working copy has unfinished work and needs cleanup.
    WcNeedsCleanup,
    /// Working copy format is not supported.
    WcFormatUnsupported,
    /// Path is not inside a working copy.
    NotAWorkingCopy,
    /// Unresolved conflicts block the commit.
    ConflictBlocksCommit,
    /// A repository hook rejected the operation (pre-commit, pre-lock, pre-revprop-change;
    /// Subversion error 165001). The hook's own output is the error message and is shown to the user.
    HookFailed,
    /// The server redirected a write (301/302/307/308): the repository moved
    /// (Subversion `SVN_ERR_RA_SESSION_URL_MISMATCH`, "Repository moved permanently"). The new
    /// location is in the error message; it was validated as an https URL on the same scheme
    /// (`RepoUrl::parse`) before being reported.
    RepositoryMoved,
    /// Path does not exist.
    PathNotFound,
    /// Path failed safety validation (traversal, reserved name, symlink).
    UnsafePath,
    /// No space left on device.
    DiskFull,
    /// OS denied access.
    PermissionDenied,
    /// File is in use by another process.
    FileInUse,
    /// Operation was cancelled.
    Cancelled,
    /// Data on disk or on the wire is corrupt or violates a protocol rule.
    Corrupt,
    /// Server lacks a required capability.
    ServerUnsupported,
    /// Bug or unclassified failure.
    Internal,
}

/// Engine error with an optional chain of causes, mirroring `svn_error_t`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct SvnError {
    /// Classification.
    pub code: ErrorCode,
    /// Human-readable detail (never contains credentials).
    pub message: String,
    /// Original Subversion error code when the error came from a server.
    pub svn_code: Option<u32>,
    /// Underlying cause.
    #[source]
    pub child: Option<Box<SvnError>>,
}

/// Result alias used across the engine.
pub type Result<T> = std::result::Result<T, SvnError>;

impl SvnError {
    /// Creates an error without a cause.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            svn_code: None,
            child: None,
        }
    }

    /// Attaches a cause.
    #[must_use]
    pub fn with_child(mut self, child: SvnError) -> Self {
        self.child = Some(Box::new(child));
        self
    }

    /// Sets the Subversion error code.
    #[must_use]
    pub fn with_svn_code(mut self, svn_code: u32) -> Self {
        self.svn_code = Some(svn_code);
        self
    }

    /// Wraps an I/O error, classifying common kinds.
    pub fn from_io(context: impl fmt::Display, err: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        let code = match err.kind() {
            ErrorKind::NotFound => ErrorCode::PathNotFound,
            ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
            ErrorKind::StorageFull | ErrorKind::QuotaExceeded => ErrorCode::DiskFull,
            ErrorKind::TimedOut => ErrorCode::Timeout,
            ErrorKind::ResourceBusy => ErrorCode::FileInUse,
            // ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: another process holds the file.
            _ if cfg!(windows) && matches!(err.raw_os_error(), Some(32 | 33)) => {
                ErrorCode::FileInUse
            }
            _ => ErrorCode::Internal,
        };
        Self::new(code, format!("{context}: {err}"))
    }
}
