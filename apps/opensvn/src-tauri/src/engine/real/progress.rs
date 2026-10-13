//! Maps `svn-client` notifications to the app's progress samples.
//!
//! `svn-client` reports one [`svn_client::UpdateEvent`] per path it touches while it applies an
//! update drive. The engine turns each one into a [`ProgressUpdate`] whose `current` counts the
//! paths handled so far. The total is unknown up front (the server streams the tree), so `total`
//! stays empty. One callback is built per operation and shared by every drive of that operation,
//! so the count keeps rising when an update runs several paths one after another.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::context::{ProgressReporter, ProgressUpdate};

/// Phase label of a checkout.
pub(super) const PHASE_CHECKOUT: &str = "checkout";
/// Phase label of an update or a switch.
pub(super) const PHASE_UPDATE: &str = "update";

/// Progress callback for `svn-client` that forwards every notification to `reporter`.
///
/// `current` is the number of notifications so far (1-based, strictly increasing) and
/// `current_path` the working copy relative path of the notification (`""` for the root).
pub(super) fn client_progress(
    reporter: &ProgressReporter,
    phase: &'static str,
) -> svn_client::ProgressFn {
    let reporter = reporter.clone();
    let count = AtomicU64::new(0);
    Arc::new(move |event: &svn_client::UpdateEvent| {
        // fetch_add hands out each number once, so samples stay ordered per caller thread.
        let current = count.fetch_add(1, Ordering::Relaxed) + 1;
        reporter.report(ProgressUpdate {
            phase: phase.to_owned(),
            current: Some(current),
            current_path: Some(event.path.as_str().to_owned()),
            ..ProgressUpdate::default()
        });
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use svn_client::{UpdateAction, UpdateEvent};
    use svn_types::{NodeKind, RelPath};

    use super::*;

    #[test]
    fn counts_events_and_keeps_paths() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let reporter = ProgressReporter::new(move |u| sink.lock().unwrap().push(u));
        let progress = client_progress(&reporter, PHASE_UPDATE);
        for path in ["", "a.txt", "src/lib.rs"] {
            progress(&UpdateEvent {
                path: RelPath::new(path).unwrap(),
                kind: NodeKind::File,
                action: UpdateAction::Add,
            });
        }
        let seen = seen.lock().unwrap();
        let currents: Vec<_> = seen.iter().map(|u| u.current).collect();
        assert_eq!(currents, [Some(1), Some(2), Some(3)]);
        assert!(
            seen.iter()
                .all(|u| u.phase == "update" && u.total.is_none())
        );
        assert_eq!(seen[2].current_path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn a_noop_reporter_is_fine() {
        let progress = client_progress(&ProgressReporter::noop(), PHASE_CHECKOUT);
        progress(&UpdateEvent {
            path: RelPath::root(),
            kind: NodeKind::Dir,
            action: UpdateAction::Update,
        });
    }
}
