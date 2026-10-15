//! The two sides of a diff target as files: what the Office text diff, "open this version" and
//! the external diff tool work from. The engine streams them, so their size does not matter.

use std::path::PathBuf;

use opensvn_ipc::{AppError, DiffTarget};

use crate::app_state::AppState;
use crate::engine::{ExportedSides, OpContext, SideFiles};

/// Path the target is shown as: the working copy path, or the last segment of the URL.
pub(crate) fn target_path(target: &DiffTarget) -> String {
    match target {
        DiffTarget::Wc { path, .. } | DiffTarget::WcVsRev { path, .. } => path.clone(),
        DiffTarget::Rev { url, .. } | DiffTarget::Range { url, .. } => {
            let last = url.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            crate::engine::text::percent_decode(last)
        }
    }
}

fn to<'a>(l: &'a Option<PathBuf>, r: &'a Option<PathBuf>) -> SideFiles<'a> {
    SideFiles {
        left: l.as_deref(),
        right: r.as_deref(),
    }
}

/// Writes the requested sides of `target` to `left` / `right` (files that must not exist yet).
/// Working copy targets are read through the working copy's actor.
pub(crate) async fn export_target(
    state: &AppState,
    target: &DiffTarget,
    left: Option<PathBuf>,
    right: Option<PathBuf>,
) -> Result<ExportedSides, AppError> {
    let detached = OpContext::detached;
    match target.clone() {
        DiffTarget::Wc { wc_id, path } => {
            state
                .wc_read(&wc_id, |engine, wc| {
                    let (path, l, r) = (path.clone(), left.clone(), right.clone());
                    async move {
                        engine
                            .export_wc_sides(&wc, &path, None, to(&l, &r), &detached())
                            .await
                    }
                })
                .await
        }
        DiffTarget::WcVsRev { wc_id, path, rev } => {
            state
                .wc_read(&wc_id, |engine, wc| {
                    let (path, l, r) = (path.clone(), left.clone(), right.clone());
                    async move {
                        engine
                            .export_wc_sides(&wc, &path, Some(rev), to(&l, &r), &detached())
                            .await
                    }
                })
                .await
        }
        DiffTarget::Rev { url, rev } => state
            .engine
            .export_rev_sides(&url, None, rev, to(&left, &right), &detached())
            .await
            .map_err(|e| AppError::from(&e)),
        DiffTarget::Range {
            url,
            from,
            to: to_rev,
        } => state
            .engine
            .export_rev_sides(&url, Some(from), to_rev, to(&left, &right), &detached())
            .await
            .map_err(|e| AppError::from(&e)),
    }
}
