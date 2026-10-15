//! Waiting for the network when a read-only drive (checkout, update) loses its connection.
//!
//! A broken connection is resumed at once by the drive (see `update::run`). When the server
//! cannot be reached at all, a Wi-Fi drop, a VPN reconnecting, a laptop waking up or a server
//! restart, resuming right away only burns the attempts. The drive then waits here: it probes the
//! server ([`RaSession::probe`]) with a capped exponential backoff for up to
//! [`NetworkWait::max_wait`], and resumes at the same revision once the server answers.
//!
//! Writes (commit, lock) never wait: whether a broken write took effect is unknown, and that
//! is for the user to decide.

use std::sync::Arc;
use std::time::{Duration, Instant};

use svn_ra_api::RaSession;
use svn_types::{CancelToken, ErrorCode, Result, SvnError};
use tokio::sync::Notify;

/// Default for [`NetworkWait::max_wait`].
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// What the drive is doing while the connection is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitState {
    /// The server did not answer; the next probe is in `retry_in` (or sooner on
    /// [`NetworkWait::retry_now`]). `waited` counts from the moment the connection was lost.
    Waiting {
        /// Time until the next probe.
        retry_in: Duration,
        /// Time spent waiting so far.
        waited: Duration,
        /// [`NetworkWait::max_wait`].
        max_wait: Duration,
        /// Probes made so far.
        attempt: u32,
    },
    /// A probe is on its way.
    Probing {
        /// 1 for the first probe.
        attempt: u32,
    },
    /// The server answered: the drive resumes.
    Restored,
    /// [`NetworkWait::max_wait`] passed without an answer: the drive stops, the working copy
    /// stays incomplete and can be continued later.
    GaveUp,
}

/// Callback for [`WaitState`] changes.
pub type WaitStateFn = Arc<dyn Fn(WaitState) + Send + Sync>;

/// How a drive waits for the network. Without it ([`crate::ClientCtx::network_wait`] is
/// `None`) a drive that cannot reconnect fails as before.
#[derive(Clone)]
pub struct NetworkWait {
    /// Longest wait for one outage.
    pub max_wait: Duration,
    /// First delay between probes; doubled after each probe up to [`Self::max_delay`].
    pub initial_delay: Duration,
    /// Longest delay between probes.
    pub max_delay: Duration,
    /// Longest time one probe may take (a dead network often swallows packets silently).
    pub probe_timeout: Duration,
    /// Wakes a waiting drive for an immediate probe: the user's "Retry now", or the system
    /// waking up from sleep.
    pub retry_now: Arc<Notify>,
    /// Told about every state change.
    pub on_state: Option<WaitStateFn>,
}

impl std::fmt::Debug for NetworkWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkWait")
            .field("max_wait", &self.max_wait)
            .field("initial_delay", &self.initial_delay)
            .field("max_delay", &self.max_delay)
            .finish_non_exhaustive()
    }
}

impl Default for NetworkWait {
    fn default() -> Self {
        Self {
            max_wait: DEFAULT_MAX_WAIT,
            initial_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            probe_timeout: Duration::from_secs(15),
            retry_now: Arc::new(Notify::new()),
            on_state: None,
        }
    }
}

impl NetworkWait {
    fn emit(&self, state: WaitState) {
        if let Some(f) = &self.on_state {
            f(state);
        }
    }
}

/// Result of [`wait_for_network`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkOutcome {
    /// The server answered the first probe: the network is fine, the failure was the server's
    /// or a proxy's (it broke the response), and the normal resume rules apply.
    Reachable,
    /// The server could not be reached, and answered again within the wait.
    Restored,
    /// The server could not be reached within [`NetworkWait::max_wait`].
    GaveUp,
}

/// A probe failure that means the server is still out of reach.
fn still_down(e: &SvnError) -> bool {
    e.is_connection_class() || matches!(e.code, ErrorCode::Network | ErrorCode::Timeout)
}

/// Probes once; `Ok(false)` while the server is out of reach. Any other answer, an error
/// included (a refused sign-in, a missing path), means the network is back.
async fn probe(session: &dyn RaSession, wait: &NetworkWait, cancel: &CancelToken) -> Result<bool> {
    match tokio::time::timeout(wait.probe_timeout, session.probe(cancel)).await {
        Err(_) => Ok(false),
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) if e.code == ErrorCode::Cancelled => Err(e),
        Ok(Err(e)) if still_down(&e) => {
            tracing::debug!(code = ?e.code, "the server is still out of reach");
            Ok(false)
        }
        Ok(Err(e)) => {
            tracing::debug!(code = ?e.code, "the server answered the probe with an error");
            Ok(true)
        }
    }
}

/// After a connection failure: probes the server, and waits for it while it cannot be
/// reached. Fails only on cancellation.
pub async fn wait_for_network(
    session: &dyn RaSession,
    wait: &NetworkWait,
    cancel: &CancelToken,
) -> Result<NetworkOutcome> {
    // Pooled connections may be dead: a probe on one would only wait for the read timeout.
    session.reset_connections();
    let start = Instant::now();
    let mut attempt = 1u32;
    wait.emit(WaitState::Probing { attempt });
    if probe(session, wait, cancel).await? {
        return Ok(NetworkOutcome::Reachable);
    }
    let mut delay = wait.initial_delay;
    loop {
        session.reset_connections();
        let waited = start.elapsed();
        if waited >= wait.max_wait {
            wait.emit(WaitState::GaveUp);
            tracing::info!(
                waited_s = waited.as_secs(),
                "the server stayed out of reach; giving up"
            );
            return Ok(NetworkOutcome::GaveUp);
        }
        let retry_in = delay.min(wait.max_wait - waited);
        wait.emit(WaitState::Waiting {
            retry_in,
            waited,
            max_wait: wait.max_wait,
            attempt,
        });
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                cancel.check()?;
            }
            () = wait.retry_now.notified() => {}
            () = tokio::time::sleep(retry_in) => {}
        }
        attempt += 1;
        wait.emit(WaitState::Probing { attempt });
        if probe(session, wait, cancel).await? {
            wait.emit(WaitState::Restored);
            tracing::info!(
                waited_s = start.elapsed().as_secs(),
                "the server is reachable again; resuming"
            );
            return Ok(NetworkOutcome::Restored);
        }
        delay = (delay * 2).min(wait.max_delay);
    }
}
