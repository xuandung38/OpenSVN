//! Maps `svn-client` notifications to the app's progress samples.
//!
//! `svn-client` reports one [`svn_client::UpdateEvent`] per path it touches while it applies an
//! update drive. The engine turns each one into a [`ProgressUpdate`] whose `current` counts the
//! paths handled so far and whose `entry` carries the path with its action (svn's `A`/`U`/`D`/...
//! column) for the path log. The total is unknown up front (the server streams the tree), so `total`
//! stays empty. One callback is built per operation and shared by every drive of that operation,
//! so the count keeps rising when an update runs several paths one after another.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use opensvn_ipc::{PathAction, ProgressEntry};
use svn_client::UpdateAction;

use crate::engine::context::{PHASE_NESTED_WC_SKIPPED, ProgressReporter, ProgressUpdate};

/// Phase label of a checkout.
pub(super) const PHASE_CHECKOUT: &str = "checkout";
/// Phase label of an update or a switch.
pub(super) const PHASE_UPDATE: &str = "update";

/// Reports every W150002 warning of an add as a [`PHASE_NESTED_WC_SKIPPED`] notice (and logs it).
pub(super) fn report_skipped_nested_wcs(
    reporter: &ProgressReporter,
    warnings: &[svn_types::SvnError],
) {
    for w in warnings {
        tracing::warn!(warning = %w, "add skipped a nested working copy");
        reporter.report(ProgressUpdate {
            phase: PHASE_NESTED_WC_SKIPPED.to_owned(),
            current_path: Some(w.message.clone()),
            ..ProgressUpdate::default()
        });
    }
}

/// IPC action of an `svn-client` notification.
pub(super) fn path_action(action: UpdateAction) -> PathAction {
    match action {
        UpdateAction::Add => PathAction::Added,
        UpdateAction::Delete => PathAction::Deleted,
        UpdateAction::Update => PathAction::Updated,
        UpdateAction::Merged => PathAction::Merged,
        UpdateAction::Conflicted => PathAction::Conflicted,
        UpdateAction::TreeConflicted => PathAction::TreeConflicted,
        UpdateAction::Existed => PathAction::Existed,
        UpdateAction::Restored => PathAction::Restored,
        UpdateAction::Skipped => PathAction::Skipped,
    }
}

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
        let path = event.path.as_str().to_owned();
        reporter.report(ProgressUpdate {
            phase: phase.to_owned(),
            current: Some(current),
            current_path: Some(path.clone()),
            entry: Some(ProgressEntry {
                action: path_action(event.action),
                path,
            }),
            ..ProgressUpdate::default()
        });
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use svn_client::UpdateEvent;
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
        let entry = seen[2].entry.clone().unwrap();
        assert_eq!(entry.action, PathAction::Added);
        assert_eq!(entry.path, "src/lib.rs");
    }

    #[test]
    fn every_action_has_its_ipc_twin() {
        let pairs = [
            (UpdateAction::Add, PathAction::Added),
            (UpdateAction::Delete, PathAction::Deleted),
            (UpdateAction::Update, PathAction::Updated),
            (UpdateAction::Merged, PathAction::Merged),
            (UpdateAction::Conflicted, PathAction::Conflicted),
            (UpdateAction::TreeConflicted, PathAction::TreeConflicted),
            (UpdateAction::Existed, PathAction::Existed),
            (UpdateAction::Restored, PathAction::Restored),
            (UpdateAction::Skipped, PathAction::Skipped),
        ];
        for (from, to) in pairs {
            assert_eq!(path_action(from), to);
        }
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
