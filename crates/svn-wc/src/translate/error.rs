//! Errors raised by translation, special-file handling and permission helpers.

use std::path::PathBuf;

use svn_types::{ErrorCode, SvnError};

/// Failure while translating content or applying file flags.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// Source mixes line-ending styles and repair was not requested (`SVN_ERR_IO_INCONSISTENT_EOL`).
    #[error("file has inconsistent newlines")]
    InconsistentEol,
    /// `svn:eol-style` holds a value svn does not define (`SVN_ERR_IO_UNKNOWN_EOL`).
    #[error("unknown svn:eol-style value")]
    UnknownEol,
    /// Stream read or write failed while translating.
    #[error("{op} failed: {source}")]
    Stream {
        /// What was attempted.
        op: &'static str,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Failure reported by a shared svn-types helper.
    #[error(transparent)]
    Svn(#[from] SvnError),
    /// Filesystem operation failed.
    #[error("{op} '{}': {source}", path.display())]
    Io {
        /// What was attempted.
        op: &'static str,
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl TranslateError {
    /// Builds an [`TranslateError::Io`] closure for `map_err`.
    pub(crate) fn io(
        op: &'static str,
        path: &std::path::Path,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io { op, path, source }
    }
}

impl From<TranslateError> for SvnError {
    fn from(err: TranslateError) -> Self {
        match err {
            // SVN_ERR_IO_INCONSISTENT_EOL and SVN_ERR_IO_UNKNOWN_EOL.
            TranslateError::InconsistentEol => {
                SvnError::new(ErrorCode::Corrupt, err.to_string()).with_svn_code(135_000)
            }
            TranslateError::UnknownEol => {
                SvnError::new(ErrorCode::Corrupt, err.to_string()).with_svn_code(135_001)
            }
            TranslateError::Stream { op, ref source } => SvnError::from_io(op, source),
            TranslateError::Io {
                op,
                ref path,
                ref source,
            } => SvnError::from_io(format!("{op} '{}'", path.display()), source),
            TranslateError::Svn(e) => e,
        }
    }
}
