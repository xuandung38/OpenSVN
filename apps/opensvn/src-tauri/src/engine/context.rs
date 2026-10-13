//! Per-call context handed to engine operations: cancellation plus progress reporting.

use std::sync::Arc;

use svn_types::{CancelToken, ErrorCode, SvnError};

/// Progress sample reported by an engine operation. The operation manager adds the operation id
/// and throttles delivery (at most 10 events per second per operation).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// Phase label such as `network` or `work-queue`.
    pub phase: String,
    /// Items done.
    pub current: Option<u64>,
    /// Items in total.
    pub total: Option<u64>,
    /// Bytes transferred.
    pub bytes_done: Option<u64>,
    /// Bytes expected.
    pub bytes_total: Option<u64>,
    /// Path being processed, relative to the working copy root.
    pub current_path: Option<String>,
}

/// Sink for progress samples; cheap to clone.
#[derive(Clone)]
pub struct ProgressReporter(Option<Arc<dyn Fn(ProgressUpdate) + Send + Sync>>);

impl ProgressReporter {
    /// Reporter that forwards samples to `f`.
    pub fn new(f: impl Fn(ProgressUpdate) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }

    /// Reporter that drops every sample.
    pub fn noop() -> Self {
        Self(None)
    }

    /// Reports one sample.
    pub fn report(&self, update: ProgressUpdate) {
        if let Some(f) = &self.0 {
            f(update);
        }
    }
}

/// Cancellation token and progress sink of one engine call.
#[derive(Clone)]
pub struct OpContext {
    /// Fires when the user cancels. Engines check it at their safe points.
    pub cancel: CancelToken,
    /// Where to report progress.
    pub progress: ProgressReporter,
    /// False for background work (periodic checks): the engine must not prompt the user and fails
    /// with `auth-required` instead when credentials are missing.
    pub interactive: bool,
}

impl OpContext {
    /// Context for short calls: never cancelled by anyone, progress dropped.
    pub fn detached() -> Self {
        Self {
            cancel: CancelToken::new(),
            progress: ProgressReporter::noop(),
            interactive: true,
        }
    }

    /// Context for unattended work: like [`OpContext::detached`] but never prompts.
    pub fn background() -> Self {
        Self {
            interactive: false,
            ..Self::detached()
        }
    }

    /// Fails with [`ErrorCode::Cancelled`] once cancellation was requested.
    pub fn check(&self) -> Result<(), SvnError> {
        self.cancel.check()
    }
}

/// Error for a cancelled operation.
pub fn cancelled() -> SvnError {
    SvnError::new(ErrorCode::Cancelled, "operation cancelled")
}
