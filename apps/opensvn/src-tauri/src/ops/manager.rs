//! Operation manager: ids, lifecycle events, throttled progress, cancellation and history.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use futures_util::FutureExt;
use opensvn_ipc::{
    AppError, ErrorCode, OpFailed, OpFinished, OpId, OpResult, OperationHandle, OperationKind,
    Progress, Size, WcId,
};
use svn_types::CancelToken;

use super::events::AppEvents;
use super::throttle::Throttle;
use crate::boundary::panic_to_error;
use crate::engine::{OpContext, ProgressReporter, ProgressUpdate};

/// How many finished operations `op_list` keeps.
pub const HISTORY_LIMIT: usize = 50;

struct Running {
    handle: OperationHandle,
    cancel: CancelToken,
    /// Newest progress sample that the throttle held back; flushed when the operation ends so the
    /// UI never stops short of the last reported state.
    pending_progress: Arc<Mutex<Option<Progress>>>,
}

#[derive(Default)]
struct State {
    running: HashMap<String, Running>,
    /// Start order of running operations.
    order: Vec<String>,
    /// Finished operations, newest first.
    finished: VecDeque<OperationHandle>,
}

struct Inner {
    events: Arc<dyn AppEvents>,
    state: Mutex<State>,
}

/// Creates, tracks and cancels operations. Cheap to clone.
#[derive(Clone)]
pub struct OperationManager {
    inner: Arc<Inner>,
}

/// New operation id (UUID v7: time-ordered).
pub fn new_op_id() -> OpId {
    OpId(uuid::Uuid::now_v7().to_string())
}

fn to_progress(op_id: &OpId, u: ProgressUpdate) -> Progress {
    Progress {
        op_id: op_id.clone(),
        phase: u.phase,
        current: u.current.map(Size),
        total: u.total.map(Size),
        bytes_done: u.bytes_done.map(Size),
        bytes_total: u.bytes_total.map(Size),
        current_path: u.current_path,
    }
}

impl OperationManager {
    /// Manager that publishes lifecycle events to `events`.
    pub fn new(events: Arc<dyn AppEvents>) -> Self {
        Self {
            inner: Arc::new(Inner {
                events,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts `work` in the background and returns its handle immediately (after emitting
    /// `op://started`). The future gets a context with the operation's cancel token and a
    /// throttled progress reporter. A panic inside `work` fails the operation, nothing else.
    pub fn spawn<F, Fut>(
        &self,
        kind: OperationKind,
        wc_id: Option<WcId>,
        title: impl Into<String>,
        work: F,
    ) -> OperationHandle
    where
        F: FnOnce(OpContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OpResult, AppError>> + Send + 'static,
    {
        let handle = OperationHandle {
            op_id: new_op_id(),
            kind,
            wc_id,
            title: title.into(),
        };
        let cancel = CancelToken::new();
        let pending_progress: Arc<Mutex<Option<Progress>>> = Arc::new(Mutex::new(None));
        {
            let mut st = self.state();
            st.order.push(handle.op_id.0.clone());
            st.running.insert(
                handle.op_id.0.clone(),
                Running {
                    handle: handle.clone(),
                    cancel: cancel.clone(),
                    pending_progress: Arc::clone(&pending_progress),
                },
            );
        }
        self.inner.events.op_started(&handle);

        let events = Arc::clone(&self.inner.events);
        let throttle = Mutex::new(Throttle::new());
        let progress_id = handle.op_id.clone();
        let pending_for_reporter = Arc::clone(&pending_progress);
        let progress_events = Arc::clone(&events);
        let ctx = OpContext {
            cancel,
            progress: ProgressReporter::new(move |u| {
                let sample = to_progress(&progress_id, u);
                let allowed = throttle
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .allow(Instant::now());
                let mut held = pending_for_reporter
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if allowed {
                    *held = None;
                    drop(held);
                    progress_events.op_progress(&sample);
                } else {
                    *held = Some(sample);
                }
            }),
        };
        let manager = self.clone();
        let task_handle = handle.clone();
        tokio::spawn(async move {
            let outcome = match AssertUnwindSafe(async move { work(ctx).await })
                .catch_unwind()
                .await
            {
                Ok(r) => r,
                Err(payload) => Err(panic_to_error(payload.as_ref())),
            };
            manager.finish(&task_handle, outcome);
        });
        handle
    }

    fn finish(&self, handle: &OperationHandle, outcome: Result<OpResult, AppError>) {
        let held = self.state().running.get(&handle.op_id.0).and_then(|r| {
            r.pending_progress
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        });
        if let Some(last) = held {
            self.inner.events.op_progress(&last);
        }
        {
            let mut st = self.state();
            st.running.remove(&handle.op_id.0);
            st.order.retain(|id| *id != handle.op_id.0);
            st.finished.push_front(handle.clone());
            st.finished.truncate(HISTORY_LIMIT);
        }
        match outcome {
            Ok(result) => self.inner.events.op_finished(&OpFinished {
                op_id: handle.op_id.clone(),
                result,
            }),
            Err(error) => self.inner.events.op_failed(&OpFailed {
                op_id: handle.op_id.clone(),
                error,
            }),
        }
    }

    /// Requests cancellation. Cancelling a finished operation is a no-op; an unknown id is an error.
    pub fn cancel(&self, op_id: &OpId) -> Result<(), AppError> {
        let st = self.state();
        if let Some(r) = st.running.get(&op_id.0) {
            r.cancel.cancel();
            return Ok(());
        }
        if st.finished.iter().any(|h| h.op_id == *op_id) {
            return Ok(());
        }
        let mut e = AppError::internal("Unknown operation.").with_detail(op_id.0.clone());
        e.code = ErrorCode::Internal;
        Err(e)
    }

    /// Running operations (oldest first) followed by the most recent finished ones (newest first).
    pub fn list(&self) -> Vec<OperationHandle> {
        let st = self.state();
        st.order
            .iter()
            .filter_map(|id| st.running.get(id).map(|r| r.handle.clone()))
            .chain(st.finished.iter().cloned())
            .collect()
    }
}
