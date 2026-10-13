//! Work queue errors.

use std::path::PathBuf;

use svn_types::{ErrorCode, SvnError};

use crate::pristine::PristineError;
use crate::translate::TranslateError;

/// `SVN_ERR_WC_BAD_ADM_LOG`, the code svn reports for unknown or failed work items.
const SVN_ERR_WC_BAD_ADM_LOG: u32 = 155_009;

/// Failure while decoding or running work items.
#[derive(Debug, thiserror::Error)]
pub enum WorkQueueError {
    /// The queue holds an operation this client does not implement; nothing was changed.
    /// The user must complete the operation with `svn cleanup`.
    #[error(
        "unrecognized work item '{0}' in the queue; finish the interrupted operation with 'svn cleanup', using the Subversion client that created this working copy"
    )]
    UnknownItem(String),
    /// The item has the wrong shape (arity, atom/list mix, non-numeric integer).
    #[error("malformed work item '{op}': {why}")]
    Malformed {
        /// Operation name.
        op: String,
        /// What is wrong.
        why: String,
    },
    /// A relpath in the item is not a safe path inside the working copy.
    #[error("unsafe path in work item: {0}")]
    UnsafePath(String),
    /// Filesystem failure.
    #[error("{op} '{}': {source}", path.display())]
    Io {
        /// What was attempted.
        op: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Translation failed.
    #[error(transparent)]
    Translate(#[from] TranslateError),
    /// Pristine store failed.
    #[error(transparent)]
    Pristine(#[from] PristineError),
    /// A database-backed callback failed.
    #[error(transparent)]
    Svn(#[from] SvnError),
    /// An item failed while running; the item stays in the queue.
    #[error("failed to run work item {id} {skel}: {source}")]
    ItemFailed {
        /// Row id of the item.
        id: u64,
        /// The item, in skel form.
        skel: String,
        /// Why it failed.
        #[source]
        source: Box<WorkQueueError>,
    },
}

impl WorkQueueError {
    pub(crate) fn io(
        op: &'static str,
        path: &std::path::Path,
    ) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.to_path_buf();
        move |source| Self::Io { op, path, source }
    }

    pub(crate) fn malformed(op: &str, why: impl Into<String>) -> Self {
        Self::Malformed {
            op: op.to_string(),
            why: why.into(),
        }
    }
}

impl From<WorkQueueError> for SvnError {
    fn from(err: WorkQueueError) -> Self {
        match err {
            WorkQueueError::UnknownItem(_) => {
                SvnError::new(ErrorCode::WcNeedsCleanup, err.to_string())
                    .with_svn_code(SVN_ERR_WC_BAD_ADM_LOG)
            }
            WorkQueueError::Malformed { .. } => SvnError::new(ErrorCode::Corrupt, err.to_string())
                .with_svn_code(SVN_ERR_WC_BAD_ADM_LOG),
            WorkQueueError::UnsafePath(_) => SvnError::new(ErrorCode::UnsafePath, err.to_string()),
            WorkQueueError::Io {
                op,
                ref path,
                ref source,
            } => SvnError::from_io(format!("{op} '{}'", path.display()), source),
            WorkQueueError::Translate(e) => e.into(),
            WorkQueueError::Pristine(e) => e.into(),
            WorkQueueError::Svn(e) => e,
            WorkQueueError::ItemFailed { source, .. } => {
                let code = SvnError::from(*source);
                SvnError::new(
                    code.code,
                    format!("work queue item failed: {}", code.message),
                )
                .with_svn_code(SVN_ERR_WC_BAD_ADM_LOG)
                .with_child(code)
            }
        }
    }
}
