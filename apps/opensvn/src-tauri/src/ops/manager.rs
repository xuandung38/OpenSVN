//! Operation manager: ids, lifecycle events, coalesced progress, cancellation and history.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use futures_util::FutureExt;
use opensvn_ipc::{
    AppError, ErrorCode, OpFailed, OpFinished, OpId, OpResult, OperationHandle, OperationKind, WcId,
};
use svn_types::CancelToken;

use super::coalesce::{Coalescer, notice};
use super::events::AppEvents;
use super::throttle::{PROGRESS_INTERVAL, Throttle};
use crate::boundary::panic_to_error;
use crate::engine::{OpContext, ProgressReporter};

/// How many finished operations `op_list` keeps.
pub const HISTORY_LIMIT: usize = 50;

struct Running {
    handle: OperationHandle,
    cancel: CancelToken,
    /// Canonical working copy folder the operation works on, if any.
    root: Option<PathBuf>,
    /// Merged progress not sent yet; flushed when the operation ends so the UI never stops short
    /// of the last reported state.
    progress: Arc<Mutex<Coalescer>>,
    /// Set once the operation ended: the progress ticker stops and sends nothing more.
    done: Arc<AtomicBool>,
    /// Wakes the operation when it waits for a lost connection (see [`OpContext::retry_now`]).
    retry_now: Arc<tokio::sync::Notify>,
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

/// What to start: the fields of the handle plus the working copy folder.
#[derive(Debug, Clone)]
pub struct OpSpec {
    /// Operation kind.
    pub kind: OperationKind,
    /// Registered working copy, if any.
    pub wc_id: Option<WcId>,
    /// Working copy folder the operation works on (a checkout's target folder).
    pub root: Option<PathBuf>,
    /// Display title.
    pub title: String,
    /// Started by the app, not by the user.
    pub background: bool,
}

impl OpSpec {
    /// A user operation without a folder.
    pub fn new(kind: OperationKind, wc_id: Option<WcId>, title: impl Into<String>) -> Self {
        Self {
            kind,
            wc_id,
            root: None,
            title: title.into(),
            background: false,
        }
    }

    /// Sets the working copy folder.
    #[must_use]
    pub fn at(mut self, root: &Path) -> Self {
        self.root = Some(canonical(root));
        self
    }

    /// Marks the operation as started by the app itself.
    #[must_use]
    pub fn in_background(mut self) -> Self {
        self.background = true;
        self
    }

    fn handle(&self) -> OperationHandle {
        OperationHandle {
            op_id: new_op_id(),
            kind: self.kind,
            wc_id: self.wc_id.clone(),
            title: self.title.clone(),
            root_path: self.root.as_ref().map(|r| r.to_string_lossy().into_owned()),
            background: self.background,
        }
    }
}

/// New operation id (UUID v7: time-ordered).
pub fn new_op_id() -> OpId {
    OpId(uuid::Uuid::now_v7().to_string())
}

/// `path` resolved (symlinks, `..`), so one folder has one spelling. A folder that does not exist
/// yet (a checkout target) is resolved through its nearest existing parent: once created, it must
/// still match the spelling of the working copy registered there.
fn canonical(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut base = path;
    loop {
        if let Ok(real) = dunce::canonicalize(base) {
            return rest.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                base = parent;
            }
            _ => return path.components().collect(),
        }
    }
}

/// Whether an operation of this kind writes to (and locks) its working copy. Reads and
/// repository-only operations never hold a working copy lock.
pub fn writes_wc(kind: OperationKind) -> bool {
    !matches!(
        kind,
        OperationKind::StatusRemote
            | OperationKind::StatusRefresh
            | OperationKind::Log
            | OperationKind::Blame
            | OperationKind::RemoteEdit
            | OperationKind::Copy
            | OperationKind::Save
    )
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
    /// coalesced progress reporter. A panic inside `work` fails the operation, nothing else.
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
        self.spawn_with(OpSpec::new(kind, wc_id, title), work)
    }

    /// [`spawn`](Self::spawn) with a working copy folder or the background flag.
    pub fn spawn_with<F, Fut>(&self, spec: OpSpec, work: F) -> OperationHandle
    where
        F: FnOnce(OpContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<OpResult, AppError>> + Send + 'static,
    {
        let handle = spec.handle();
        let cancel = CancelToken::new();
        let progress = Arc::new(Mutex::new(Coalescer::new(
            handle.op_id.clone(),
            Throttle::new(),
        )));
        let done = Arc::new(AtomicBool::new(false));
        let retry_now = Arc::new(tokio::sync::Notify::new());
        {
            let mut st = self.state();
            st.order.push(handle.op_id.0.clone());
            st.running.insert(
                handle.op_id.0.clone(),
                Running {
                    handle: handle.clone(),
                    cancel: cancel.clone(),
                    root: spec.root.clone(),
                    progress: Arc::clone(&progress),
                    done: Arc::clone(&done),
                    retry_now: Arc::clone(&retry_now),
                },
            );
        }
        self.inner.events.op_started(&handle);

        let events = Arc::clone(&self.inner.events);
        let reporter = {
            let (progress, done, events, op_id) = (
                Arc::clone(&progress),
                Arc::clone(&done),
                Arc::clone(&events),
                handle.op_id.clone(),
            );
            ProgressReporter::new(move |u| {
                // Sent under the lock so samples leave in the order they were merged, and none
                // after `finish` flushed the last one (work that outlived its operation).
                let mut merged = progress.lock().unwrap_or_else(PoisonError::into_inner);
                if done.load(Ordering::SeqCst) {
                    return;
                }
                if u.is_notice() {
                    // Notices are for the user: delivered at once, never coalesced away.
                    events.op_progress(&notice(&op_id, u));
                    return;
                }
                if let Some(p) = merged.push(u, Instant::now()) {
                    events.op_progress(&p);
                }
            })
        };
        spawn_ticker(&progress, &done, &cancel, &events);
        let ctx = OpContext {
            interactive: !spec.background,
            cancel,
            progress: reporter,
            retry_now,
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
        let running = self.state().running.get(&handle.op_id.0).map(|r| {
            (
                Arc::clone(&r.progress),
                Arc::clone(&r.done),
                r.cancel.clone(),
            )
        });
        if let Some((progress, done, cancel)) = running {
            let mut merged = progress.lock().unwrap_or_else(PoisonError::into_inner);
            done.store(true, Ordering::SeqCst);
            merged.set_received(cancel.received_bytes());
            if let Some(last) = merged.flush() {
                self.inner.events.op_progress(&last);
            }
        }
        {
            let mut st = self.state();
            st.running.remove(&handle.op_id.0);
            st.order.retain(|id| *id != handle.op_id.0);
            // Background checks run every few minutes: they would push the user's operations
            // out of the history.
            if !handle.background {
                st.finished.push_front(handle.clone());
                st.finished.truncate(HISTORY_LIMIT);
            }
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

    /// The oldest running user operation that writes to the working copy at `root` (or to a
    /// folder inside it, or to a folder containing it). Its lock on that working copy is ours,
    /// not a stale one.
    pub fn running_on(&self, root: &Path) -> Option<OperationHandle> {
        let root = canonical(root);
        let st = self.state();
        st.order.iter().find_map(|id| {
            let r = st.running.get(id)?;
            let op_root = r.root.as_ref()?;
            let related = op_root.starts_with(&root) || root.starts_with(op_root);
            (related && !r.handle.background && writes_wc(r.handle.kind)).then(|| r.handle.clone())
        })
    }

    /// "Retry now": an operation waiting for a lost connection tries again at once. A finished
    /// operation, or one that is not waiting, ignores it; an unknown id is an error.
    pub fn retry_now(&self, op_id: &OpId) -> Result<(), AppError> {
        let st = self.state();
        if let Some(r) = st.running.get(&op_id.0) {
            r.retry_now.notify_one();
            return Ok(());
        }
        if st.finished.iter().any(|h| h.op_id == *op_id) {
            return Ok(());
        }
        Err(AppError::internal("Unknown operation.").with_detail(op_id.0.clone()))
    }

    /// Wakes every running operation that waits for a lost connection (the system woke up).
    pub fn retry_all_now(&self) {
        for r in self.state().running.values() {
            r.retry_now.notify_one();
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

/// Sends the merged progress of a running operation once per throttle interval, also when the
/// engine is quiet (a long download between two paths: the byte count still moves), and stops
/// when the operation ends.
fn spawn_ticker(
    progress: &Arc<Mutex<Coalescer>>,
    done: &Arc<AtomicBool>,
    cancel: &CancelToken,
    events: &Arc<dyn AppEvents>,
) {
    let (progress, done, cancel, events) = (
        Arc::clone(progress),
        Arc::clone(done),
        cancel.clone(),
        Arc::clone(events),
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(PROGRESS_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let mut merged = progress.lock().unwrap_or_else(PoisonError::into_inner);
            if done.load(Ordering::SeqCst) {
                return;
            }
            merged.set_received(cancel.received_bytes());
            if let Some(p) = merged.poll(Instant::now()) {
                events.op_progress(&p);
            }
        }
    });
}
