//! Byte sinks of the real engine: a size-capped async buffer for downloads and a shared,
//! size-capped sync buffer for texts rebuilt from deltas.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

/// In-memory sink that refuses to grow beyond a limit, so a huge file is never buffered whole.
pub(super) struct CappedBuf {
    buf: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedBuf {
    /// Creates a sink holding at most `limit` bytes.
    pub(super) fn new(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    /// Whether a write was refused because of the limit.
    pub(super) fn exceeded(&self) -> bool {
        self.exceeded
    }

    /// The collected bytes.
    pub(super) fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

impl AsyncWrite for CappedBuf {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.buf.len().saturating_add(data.len()) > self.limit {
            self.exceeded = true;
            return Poll::Ready(Err(std::io::Error::other("size limit exceeded")));
        }
        self.buf.extend_from_slice(data);
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Cloneable, size-capped synchronous byte sink; lets the caller read what a boxed delta
/// applier produced. A write that would grow it beyond the limit fails and is remembered.
#[derive(Clone)]
pub(super) struct SharedBuf(Arc<Mutex<SharedInner>>);

struct SharedInner {
    buf: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl SharedBuf {
    /// Creates a sink holding at most `limit` bytes.
    pub(super) fn with_limit(limit: usize) -> Self {
        Self(Arc::new(Mutex::new(SharedInner {
            buf: Vec::new(),
            limit,
            exceeded: false,
        })))
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, SharedInner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a write was refused because of the limit.
    pub(super) fn exceeded(&self) -> bool {
        self.inner().exceeded
    }

    /// Takes the collected bytes, leaving the buffer empty.
    pub(super) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.inner().buf)
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.inner();
        if inner.buf.len().saturating_add(data.len()) > inner.limit {
            inner.exceeded = true;
            return Err(std::io::Error::other("size limit exceeded"));
        }
        inner.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
