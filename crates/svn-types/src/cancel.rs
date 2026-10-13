//! Runtime-agnostic cancellation token.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use crate::error::{ErrorCode, Result, SvnError};

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
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
        for w in wakers {
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

    /// Future that completes once the token is cancelled.
    pub fn cancelled(&self) -> Cancelled {
        Cancelled(self.clone())
    }
}

/// Future returned by [`CancelToken::cancelled`].
#[derive(Debug)]
pub struct Cancelled(CancelToken);

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = &(self.0).0;
        // Registering under the lock and re-checking the flag closes the race with `cancel`,
        // which sets the flag before draining the list.
        let mut wakers = inner.wakers.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.flag.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
            wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}
