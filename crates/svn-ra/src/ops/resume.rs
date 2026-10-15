//! Reads whose streamed body breaks off on a failed connection.
//!
//! [`HttpTransport::send_and_read`](crate::transport::HttpTransport::send_and_read) covers
//! buffered answers. The reads here hand their body to a consumer while it arrives (log
//! entries, file content, file revisions) or parse it into a bounded result (listings, locks).
//! When the connection fails mid-body (see [`svn_types::ConnectionFailure`]) such a read is sent
//! again on a fresh connection:
//!
//! - nothing reached the consumer yet: the read starts over;
//! - part of it did: it resumes where the protocol allows (the next log revision, the next file
//!   byte, the next file revision) and skips what the consumer already has;
//! - otherwise it fails with the connection error, never with a partial result reported as
//!   complete.
//!
//! Only read-only requests (`GET`, `PROPFIND`, `REPORT`) go through here; writes are never sent
//! twice. The delays are the transport's retry delays, cut short by the cancel token.

use std::time::Duration;

use svn_types::{CancelToken, ErrorCode, Result, SvnError};

use super::update::RaWire;

/// Most re-requests of one read, whatever the progress in between: bounds a connection that
/// keeps breaking.
const MAX_RESUMES: usize = 20;

/// Re-request budget of one streamed read.
pub(crate) struct ReadRetry {
    delays: Vec<Duration>,
    /// Re-requests in a row without progress.
    stalled: usize,
    /// Re-requests in total.
    total: usize,
    what: &'static str,
}

impl ReadRetry {
    /// Budget for the read named `what` (for logs), with the delays of the wire's transport.
    pub(crate) fn new(wire: &RaWire, what: &'static str) -> Self {
        Self {
            delays: wire.transport().retry_policy().delays.clone(),
            stalled: 0,
            total: 0,
            what,
        }
    }

    /// After a failed attempt: waits and returns `Ok` when the read should be sent again, or
    /// returns the error. Only a connection failure (not a cancellation) is retried, at most as
    /// often as the retry delays allow in a row without progress (see [`Self::progressed`]) and
    /// [`MAX_RESUMES`] times in total. The wait ends early with `Cancelled`.
    pub(crate) async fn again(&mut self, err: SvnError, cancel: &CancelToken) -> Result<()> {
        if err.code == ErrorCode::Cancelled || err.connection_failure().is_none() {
            return Err(err);
        }
        cancel.check()?;
        let Some(delay) = self.delays.get(self.stalled).copied() else {
            return Err(err);
        };
        if self.total >= MAX_RESUMES {
            return Err(err);
        }
        self.stalled += 1;
        self.total += 1;
        tracing::debug!(
            what = self.what,
            failure = ?err.connection_failure(),
            attempt = self.total,
            "streamed read broke off on a failed connection; sending it again"
        );
        tokio::select! {
            biased;
            () = cancel.cancelled() => cancel.check()?,
            () = tokio::time::sleep(delay) => {}
        }
        Ok(())
    }

    /// The last attempt handed something new to the consumer: the next failure starts a new
    /// run of delays.
    pub(crate) fn progressed(&mut self) {
        self.stalled = 0;
    }
}

/// `err`, a connection failure after part of the answer reached the consumer, where the read
/// cannot be resumed: the code and [`SvnError::connection`] stay, the message says the result
/// is incomplete.
pub(crate) fn cannot_resume(mut err: SvnError, what: &str) -> SvnError {
    err.message = format!(
        "the {what} broke off after part of it was received and cannot be resumed; \
         nothing was completed: {}",
        err.message
    );
    err
}
