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
    /// Negotiate (Kerberos single sign-on) was offered by the server but could not be used.
    ///
    /// Returned when:
    /// - Negotiate fails (no ticket, no SPN for the host, the URL host is an IP address, the
    ///   system would only offer NTLM, the server rejected the ticket, or this build has no
    ///   Negotiate support) and the server offers no Basic fallback;
    /// - the server's mutual-authentication token fails verification, even when Basic is offered
    ///   (the response is discarded);
    /// - the repository URL is plain `http://` (tokens are only sent over https);
    /// - the user picks Negotiate in the `PromptReason::NegotiateFailed` prompt instead of
    ///   entering Basic credentials.
    ///
    /// When Negotiate fails and Basic is offered over https, the user is prompted with
    /// `PromptReason::NegotiateFailed` instead of receiving this error.
    NegotiateFailed,
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
    /// The server answered with a redirect (301/302/307/308) that the client does not follow:
    /// svn reports `SVN_ERR_RA_DAV_RELOCATED` (175011), "Repository moved permanently". Policy:
    /// same-origin redirects are followed for read methods; write methods and cross-origin
    /// redirects return this code. The new location is in the error message; it was validated
    /// as an https URL on the same scheme (`RepoUrl::parse`) before being reported.
    RepositoryMoved,
    /// The pristine (base text) of a file is not stored locally, as in a format-32 working copy
    /// made with `--store-pristine=no`. Operations that need it (diff, revert) must first fetch
    /// the base text from the repository.
    PristineNotHydrated,
    /// The operation is not valid in the working copy's current state (Subversion E1550xx
    /// refusals, e.g. E155038 "can't revert without reverting children"). The message explains
    /// what the user has to change first.
    WcInvalidOperation,
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
    /// A commit's final request (`MERGE`) reached the server, but its answer never arrived or
    /// could not be read (cancellation, a broken connection, an unreadable success response).
    /// The commit may or may not have become a revision.
    ///
    /// The server transaction is deliberately kept: deleting it could race a commit that is
    /// still being applied. The message names the transaction; callers that journaled it
    /// (`CommitObserver::on_txn_created`) reconcile later: if the youngest revision carries the
    /// commit, it happened; otherwise remove the transaction with `abort_txn`. Never retry the
    /// commit blindly: it could be applied twice.
    CommitOutcomeUnknown,
    /// A write other than a commit (a lock or an unlock) whose connection broke after the
    /// request was handed to it: the server may or may not have carried it out. Writes are
    /// never sent twice, so the caller checks the server's state before trying again.
    WriteOutcomeUnknown,
    /// The request is valid but its result would exceed a size limit the engine enforces to
    /// bound memory and work (for example a directory listing with more entries than the
    /// listing cap). Nothing is wrong with the data; the user can ask for less (a smaller
    /// subtree, a lower depth). Retrying the same request fails the same way.
    LimitExceeded,
    /// Data on disk or on the wire is corrupt or violates a protocol rule.
    Corrupt,
    /// Server lacks a required capability.
    ServerUnsupported,
    /// Bug or unclassified failure.
    Internal,
}

/// How a network connection failed, for errors with [`ErrorCode::Network`] or
/// [`ErrorCode::Timeout`] that the transport could classify. The UI uses it for a friendly text;
/// the engine uses it to retry read-only work on a fresh connection.
///
/// Every variant means the request may not have completed. Whether it took effect on the server
/// is unknown for a write; read-only requests are safe to send again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionFailure {
    /// The server (or a proxy) closed the connection before the response was complete:
    /// a reused keep-alive connection closed under the request, or an end of stream in the
    /// middle of a response.
    ConnectionClosedByServer,
    /// The connection was reset (TCP RST) or aborted, or a write found it already closed.
    ConnectionReset,
    /// No response bytes arrived within the configured read timeout.
    Timeout,
    /// The TLS stream ended without a `close_notify` alert before the response was complete
    /// ("peer closed connection without sending TLS close_notify"). Apache on Windows, IIS and
    /// many proxies close this way; when the body was already complete it is not an error.
    TlsClosedUncleanly,
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
    /// How the connection failed, when this is a classified network failure.
    pub connection: Option<ConnectionFailure>,
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
            connection: None,
            child: None,
        }
    }

    /// Records how the connection failed.
    #[must_use]
    pub fn with_connection(mut self, failure: ConnectionFailure) -> Self {
        self.connection = Some(failure);
        self
    }

    /// The connection failure of this error or of its first cause that has one.
    pub fn connection_failure(&self) -> Option<ConnectionFailure> {
        let mut e = Some(self);
        while let Some(err) = e {
            if err.connection.is_some() {
                return err.connection;
            }
            e = err.child.as_deref();
        }
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connection_failure_is_found_in_the_cause_chain() {
        let plain = SvnError::new(ErrorCode::Network, "refused");
        assert_eq!(plain.connection_failure(), None);
        let broken = SvnError::new(ErrorCode::Network, "cut off")
            .with_connection(ConnectionFailure::TlsClosedUncleanly);
        assert_eq!(
            broken.connection_failure(),
            Some(ConnectionFailure::TlsClosedUncleanly)
        );
        let wrapped = SvnError::new(ErrorCode::Internal, "drive failed").with_child(broken);
        assert_eq!(wrapped.connection, None);
        assert_eq!(
            wrapped.connection_failure(),
            Some(ConnectionFailure::TlsClosedUncleanly)
        );
    }
}
