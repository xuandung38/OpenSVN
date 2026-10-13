//! Runtime-agnostic cancellation token.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use crate::error::{ErrorCode, Result, SvnError};

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    /// Registered futures by id; an entry is removed when its future completes or is dropped.
    wakers: Mutex<Vec<(u64, Waker)>>,
    next_id: AtomicU64,
}

/// Cloneable cancellation flag usable from sync code (`check`) and any async runtime
/// (`cancelled().await`). All clones share state.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<Inner>);

impl CancelToken {
    /// Creates a token in the not-cancelled state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation and wakes every waiting future. Idempotent.
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        let wakers =
            std::mem::take(&mut *self.0.wakers.lock().unwrap_or_else(PoisonError::into_inner));
        for (_, w) in wakers {
            w.wake();
        }
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// Returns [`ErrorCode::Cancelled`] when cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(SvnError::new(ErrorCode::Cancelled, "operation cancelled"))
        } else {
            Ok(())
        }
    }

    /// Number of futures currently waiting on this token (diagnostics and tests).
    pub fn registered_wakers(&self) -> usize {
        self.0
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Future that completes once the token is cancelled.
    pub fn cancelled(&self) -> Cancelled {
        Cancelled {
            token: self.clone(),
            id: None,
        }
    }
}

/// Future returned by [`CancelToken::cancelled`]. Dropping it unregisters its waker, so a
/// long-lived token polled by many short-lived futures does not accumulate state.
#[derive(Debug)]
pub struct Cancelled {
    token: CancelToken,
    id: Option<u64>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = Arc::clone(&self.token.0);
        // Registering under the lock and re-checking the flag closes the race with `cancel`,
        // which sets the flag before draining the list.
        let mut wakers = inner.wakers.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.flag.load(Ordering::SeqCst) {
            self.id = None; // `cancel` already drained the list
            return Poll::Ready(());
        }
        match self
            .id
            .and_then(|id| wakers.iter_mut().find(|(i, _)| *i == id))
        {
            Some((_, w)) => {
                if !w.will_wake(cx.waker()) {
                    w.clone_from(cx.waker());
                }
            }
            None => {
                let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
                wakers.push((id, cx.waker().clone()));
                drop(wakers);
                self.id = Some(id);
            }
        }
        Poll::Pending
    }
}

impl Drop for Cancelled {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let mut wakers = self
                .token
                .0
                .wakers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            wakers.retain(|(i, _)| *i != id);
        }
    }
}
