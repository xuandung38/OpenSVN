//! Periodic check for new revisions of the open working copies (`general.checkIntervalMin`).

use std::sync::Arc;
use std::time::Duration;

use opensvn_ipc::Settings;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::publisher::StatusPublisher;
use super::remote_check::run_remote_check;
use crate::engine::{EngineFacade, OpContext};
use crate::notify::{Notifier, incoming_text};
use crate::wc::WcRegistry;

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

async fn check_all(deps: &PollerDeps) {
    for id in deps.wcs.open_ids() {
        let Ok(resolved) = deps.wcs.resolve(&id) else {
            continue;
        };
        // Unattended: the engine must not prompt; a missing login just skips this round.
        let ctx = OpContext::background();
        let before = deps.wcs.incoming_of(&id);
        match run_remote_check(&deps.engine, &deps.publisher, &deps.wcs, &resolved, &ctx).await {
            Ok(()) => {
                // Notify only when more revisions arrived since the last check.
                let after = deps.wcs.incoming_of(&id);
                if after > before {
                    let (title, body) = incoming_text(&resolved.row.name, after);
                    deps.notifier.notify(&title, &body);
                }
            }
            Err(e) => {
                tracing::info!(wc = %id.0, code = ?e.code, "periodic update check skipped");
            }
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
