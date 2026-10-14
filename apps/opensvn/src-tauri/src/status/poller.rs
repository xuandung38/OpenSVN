//! Periodic check for new revisions of the open working copies (`general.checkIntervalMin`).

use std::sync::Arc;
use std::time::Duration;

use opensvn_ipc::{AppError, OpResult, OperationKind, Settings};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::publisher::StatusPublisher;
use super::remote_check::run_remote_check;
use crate::engine::EngineFacade;
use crate::notify::{Notifier, incoming_text};
use crate::ops::{OpSpec, OperationManager};
use crate::wc::{ResolvedWc, WcRegistry};

/// Period between checks for a given setting.
pub fn period_from(settings: &Settings) -> Duration {
    Duration::from_secs(u64::from(settings.general_check_interval_min.max(1)) * 60)
}

/// What the poller needs.
pub struct PollerDeps {
    /// Engine used for the checks.
    pub engine: Arc<dyn EngineFacade>,
    /// Where results are announced.
    pub publisher: Arc<StatusPublisher>,
    /// Working copy registry (open set and incoming counts).
    pub wcs: Arc<WcRegistry>,
    /// Desktop notifications for new revisions.
    pub notifier: Arc<dyn Notifier>,
    /// Runs each check as a background operation, and knows which working copies are busy.
    pub ops: OperationManager,
    /// Settings changes (the interval).
    pub settings: watch::Receiver<Settings>,
    /// Fixed period instead of the setting (tests).
    pub period_override: Option<Duration>,
}

/// Handle to the polling task; dropping it does not stop the task, `stop` does.
pub struct IncomingPoller {
    task: JoinHandle<()>,
}

impl IncomingPoller {
    /// Starts polling on the current runtime.
    pub fn start(deps: PollerDeps) -> Self {
        Self {
            task: tokio::spawn(poll_loop(deps)),
        }
    }

    /// Stops polling.
    pub fn stop(&self) {
        self.task.abort();
    }
}

async fn poll_loop(mut deps: PollerDeps) {
    loop {
        let period = deps
            .period_override
            .unwrap_or_else(|| period_from(&deps.settings.borrow()));
        // Biased: a settings change (or shutdown) seen together with the timer is handled first,
        // so no check starts on a stale interval or while the app is shutting down.
        tokio::select! {
            biased;
            // A changed interval restarts the wait with the new period.
            changed = deps.settings.changed(), if deps.period_override.is_none() => {
                if changed.is_err() {
                    return; // settings service is gone: the app is shutting down
                }
            }
            () = tokio::time::sleep(period) => check_all(&deps).await,
        }
    }
}

/// Why a periodic check leaves a working copy alone this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// One of our operations (a checkout, an update, ...) is working on it.
    Busy,
    /// A checkout or update stopped half-way; the user finishes it with an update first.
    Incomplete,
}

/// Whether the periodic check must skip `wc` this round. A skipped check is retried at the next
/// round and never reported to the user.
pub async fn skip_reason(
    engine: &Arc<dyn EngineFacade>,
    ops: &OperationManager,
    wc: &ResolvedWc,
) -> Option<SkipReason> {
    if ops.running_on(&wc.wc.root).is_some() || wc.actor.is_writing() {
        return Some(SkipReason::Busy);
    }
    match engine
        .wc_info(&wc.wc.root, &crate::engine::OpContext::background())
        .await
    {
        Ok(info) if info.incomplete => Some(SkipReason::Incomplete),
        // Unreadable right now (locked by a writer, being created): try again next round.
        Err(_) => Some(SkipReason::Busy),
        Ok(_) => None,
    }
}

async fn check_all(deps: &PollerDeps) {
    for id in deps.wcs.open_ids() {
        let Ok(resolved) = deps.wcs.resolve(&id) else {
            continue;
        };
        if let Some(reason) = skip_reason(&deps.engine, &deps.ops, &resolved).await {
            tracing::debug!(wc = %id.0, ?reason, "periodic update check skipped");
            continue;
        }
        let before = deps.wcs.incoming_of(&id);
        // A background operation: unattended (never prompts; a missing login just fails this
        // round) and reported quietly by the UI.
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), AppError>>();
        let spec = OpSpec::new(
            OperationKind::StatusRemote,
            Some(id.clone()),
            format!("Check {} for updates", resolved.row.name),
        )
        .at(&resolved.wc.root)
        .in_background();
        let (engine, publisher, wcs, wc) = (
            Arc::clone(&deps.engine),
            Arc::clone(&deps.publisher),
            Arc::clone(&deps.wcs),
            resolved.clone(),
        );
        deps.ops.spawn_with(spec, move |ctx| async move {
            let result = run_remote_check(&engine, &publisher, &wcs, &wc, &ctx).await;
            let _ = tx.send(result.clone());
            result.map(|()| OpResult::Done)
        });
        match rx.await {
            Ok(Ok(())) => {
                // Notify only when more revisions arrived since the last check.
                let after = deps.wcs.incoming_of(&id);
                if after > before {
                    let (title, body) = incoming_text(&resolved.row.name, after);
                    deps.notifier.notify(&title, &body);
                }
            }
            Ok(Err(e)) => {
                tracing::info!(wc = %id.0, code = ?e.code, "periodic update check failed");
            }
            // The operation panicked before answering; already reported as failed.
            Err(_) => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::settings::defaults;

    #[test]
    fn period_follows_the_setting_and_never_drops_to_zero() {
        let mut s = defaults(None);
        assert_eq!(period_from(&s), Duration::from_secs(600));
        s.general_check_interval_min = 1;
        assert_eq!(period_from(&s), Duration::from_secs(60));
        s.general_check_interval_min = 0;
        assert_eq!(period_from(&s), Duration::from_secs(60));
    }
}
