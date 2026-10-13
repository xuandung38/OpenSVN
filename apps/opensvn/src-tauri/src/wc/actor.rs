//! Per-working-copy actor.
//!
//! * Write jobs run strictly one after another in submission order.
//! * Read jobs go through the same gate: they run concurrently with each other, never while a
//!   write job runs, and retry with backoff while a writer holds the working copy (wc.db keeps
//!   svn's rollback journal, so readers genuinely block during a write transaction).
//! * A panic inside a job fails that job only; the actor keeps serving.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::FutureExt;
use opensvn_ipc::{AppError, ErrorAction, ErrorCode};
use svn_types::CancelToken;
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::boundary::panic_to_error;

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

/// How hard a read tries before it reports the working copy as busy.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum attempts (at least 1).
    pub attempts: u32,
    /// Delay after the first failed attempt; doubles each time up to `max_delay`.
    pub base_delay: Duration,
    /// Upper bound of the delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 8,
            base_delay: Duration::from_millis(25),
            max_delay: Duration::from_millis(400),
        }
    }
}

struct Shared {
    gate: RwLock<()>,
    /// Writes submitted and not yet finished (queued or running).
    pending_writes: AtomicUsize,
}

/// Handle to the actor of one working copy. Cheap to clone via `Arc`.
pub struct WcActor {
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Job>,
    policy: RetryPolicy,
}

fn busy() -> AppError {
    let mut e = AppError::internal("The working copy is busy. Try again in a moment.");
    e.code = ErrorCode::WcBusy;
    e.retryable = true;
    e.actions = Some(vec![ErrorAction::Retry]);
    e
}

fn cancelled() -> AppError {
    AppError::from(&svn_types::SvnError::new(
        svn_types::ErrorCode::Cancelled,
        "operation cancelled",
    ))
}

fn actor_gone() -> AppError {
    AppError::internal("The working copy worker stopped unexpectedly.")
}

impl WcActor {
    /// Starts the actor task on the current Tokio runtime.
    pub fn spawn(policy: RetryPolicy) -> Self {
        let shared = Arc::new(Shared {
            gate: RwLock::new(()),
            pending_writes: AtomicUsize::new(0),
        });
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                job.await;
            }
        });
        Self { shared, tx, policy }
    }

    /// True while a write is queued or running. Status reads then answer from the cache.
    pub fn is_writing(&self) -> bool {
        self.shared.pending_writes.load(Ordering::SeqCst) > 0
    }

    /// Queues `job` behind earlier writes and runs it with exclusive access.
    pub async fn write<T, Fut>(&self, job: Fut) -> Result<T, AppError>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        self.write_cancellable(job, &CancelToken::new()).await
    }

    /// Like [`WcActor::write`], but a queued job is abandoned as soon as `cancel` fires: it never
    /// starts, and the caller gets `cancelled` immediately instead of waiting for the writes
    /// ahead of it. Once the job has started, cancellation is up to the job itself (it holds the
    /// token), so the caller keeps waiting for its real result.
    pub async fn write_cancellable<T, Fut>(
        &self,
        job: Fut,
        cancel: &CancelToken,
    ) -> Result<T, AppError>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, AppError>> + Send + 'static,
    {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let (reply_tx, mut reply_rx) = oneshot::channel();
        let shared = Arc::clone(&self.shared);
        let started = Arc::new(AtomicBool::new(false));
        let (job_started, job_cancel) = (Arc::clone(&started), cancel.clone());
        shared.pending_writes.fetch_add(1, Ordering::SeqCst);
        let wrapped: Job = Box::pin(async move {
            let result = async {
                if job_cancel.is_cancelled() {
                    return Err(cancelled());
                }
                let _exclusive = tokio::select! {
                    guard = shared.gate.write() => guard,
                    () = job_cancel.cancelled() => return Err(cancelled()),
                };
                job_started.store(true, Ordering::SeqCst);
                match AssertUnwindSafe(job).catch_unwind().await {
                    Ok(r) => r,
                    Err(p) => Err(panic_to_error(p.as_ref())),
                }
            }
            .await;
            shared.pending_writes.fetch_sub(1, Ordering::SeqCst);
            let _ = reply_tx.send(result);
        });
        if self.tx.send(wrapped).is_err() {
            self.shared.pending_writes.fetch_sub(1, Ordering::SeqCst);
            return Err(actor_gone());
        }
        loop {
            tokio::select! {
                reply = &mut reply_rx => return reply.map_err(|_| actor_gone())?,
                () = cancel.cancelled(), if !started.load(Ordering::SeqCst) => {
                    // Not started: the wrapper drops the job when it reaches it.
                    if !started.load(Ordering::SeqCst) {
                        return Err(cancelled());
                    }
                }
            }
        }
    }

    /// Runs a read. `make` builds the job again for every attempt. An attempt is retried when a
    /// writer is active or the job itself reports `wc-busy`; after the last attempt the caller gets
    /// a retryable `wc-busy` error.
    pub async fn read<T, Fut>(&self, make: impl Fn() -> Fut) -> Result<T, AppError>
    where
        Fut: Future<Output = Result<T, AppError>>,
    {
        let mut delay = self.policy.base_delay;
        for attempt in 1..=self.policy.attempts.max(1) {
            if let Ok(_shared) = self.shared.gate.try_read() {
                match AssertUnwindSafe(make()).catch_unwind().await {
                    Ok(Err(e)) if e.code == ErrorCode::WcBusy => {}
                    Ok(result) => return result,
                    Err(p) => return Err(panic_to_error(p.as_ref())),
                }
            }
            if attempt < self.policy.attempts.max(1) {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(self.policy.max_delay);
            }
        }
        Err(busy())
    }
}
