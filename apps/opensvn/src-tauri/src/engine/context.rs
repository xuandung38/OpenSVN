//! Per-call context handed to engine operations: cancellation plus progress reporting.

use std::sync::Arc;

use opensvn_ipc::{ConnectionWait, OpStage, ProgressEntry};
use svn_types::{CancelToken, ErrorCode, SvnError};

/// Progress sample reported by an engine operation. The operation manager adds the operation id
/// and coalesces delivery (at most 20 events per second per operation): fields left `None` keep
/// their last reported value, and every `entry` reaches the path log.
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
    /// Stage change.
    pub stage: Option<OpStage>,
    /// One path the operation handled, for the path log.
    pub entry: Option<ProgressEntry>,
    /// Timing of [`OpStage::WaitingForConnection`]; sent with that stage.
    pub connection_wait: Option<ConnectionWait>,
}

/// Notice phase: the operation went through, but the working copy needs `cleanup`
/// (`current_path` carries the reason). The UI shows the working copy's "needs cleanup" banner.
pub const PHASE_NEEDS_CLEANUP: &str = "needs-cleanup";
/// Notice phase: an add skipped a nested working copy (svn's W150002); `current_path` carries the
/// warning text. The UI shows it to the user.
pub const PHASE_NESTED_WC_SKIPPED: &str = "nested-wc-skipped";
/// Notice phase: an external needs attention (failed or skipped); `current_path` is its path.
pub const PHASE_EXTERNAL_WARNING: &str = "external-warning";

impl ProgressUpdate {
    /// A notice for the user rather than a progress count: never dropped by the throttle.
    pub fn is_notice(&self) -> bool {
        self.phase == PHASE_NEEDS_CLEANUP
            || self.phase == PHASE_NESTED_WC_SKIPPED
            || self.phase == PHASE_EXTERNAL_WARNING
    }

    /// A sample that only announces a new stage.
    pub fn stage(phase: &str, stage: OpStage) -> Self {
        Self {
            phase: phase.to_owned(),
            stage: Some(stage),
            ..Self::default()
        }
    }
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
    /// Wakes an operation that waits for a lost connection, for an immediate attempt: the
    /// user's "Retry now", or the system waking up from sleep.
    pub retry_now: Arc<tokio::sync::Notify>,
}

impl OpContext {
    /// Context for short calls: never cancelled by anyone, progress dropped.
    pub fn detached() -> Self {
        Self {
            cancel: CancelToken::new(),
            progress: ProgressReporter::noop(),
            interactive: true,
            retry_now: Arc::default(),
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
