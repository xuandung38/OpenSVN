//! The two sides of a diff target as files: what the Office text diff, "open this version" and
//! the external diff tool work from. The engine streams them, so their size does not matter.

use std::path::PathBuf;

use opensvn_ipc::{AppError, DiffSideKind, DiffTarget};

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

/// File name of a staged version: what it is (`base`, `working`, `r12`) plus the sanitized
/// extension of the file. Nothing else from the repository reaches the name.
pub(crate) fn version_file_name(target: &DiffTarget, side: DiffSideKind) -> String {
    let left = side == DiffSideKind::Left;
    let stem = match target {
        DiffTarget::Wc { .. } => (if left { "base" } else { "working" }).to_owned(),
        DiffTarget::WcVsRev { rev, .. } if left => format!("r{}", rev.0),
        DiffTarget::WcVsRev { .. } => "working".to_owned(),
        DiffTarget::Rev { rev, .. } if left => format!("r{}", rev.0.saturating_sub(1)),
        DiffTarget::Rev { rev, .. } => format!("r{}", rev.0),
        DiffTarget::Range { from, .. } if left => format!("r{}", from.0),
        DiffTarget::Range { to, .. } => format!("r{}", to.0),
    };
    match crate::programs::external_diff::staged_extension(&target_path(target)) {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem,
    }
}

/// Writes one version of `target` as a read-only copy in the private external-diff folder and
/// opens it with the default application (Word, Excel, Numbers...), e.g. to use Word's own
/// Compare. Only the document types the default application may open at all are accepted
/// (`os_integration`), checked before anything is downloaded and again on the copy.
pub(crate) async fn open_version(
    state: &AppState,
    target: DiffTarget,
    side: DiffSideKind,
) -> Result<(), AppError> {
    use crate::os_integration::{default_app_allowed, default_app_name_allowed};
    use crate::programs::external_diff::{STALE_AFTER, make_read_only, new_stage_dir, sweep_stale};
    let name = version_file_name(&target, side);
    let refused = || {
        crate::errors::invalid_argument(
            "This type of file is not opened with the default app. Use the external diff tool instead.",
        )
        .with_path(target_path(&target))
    };
    if !default_app_name_allowed(&name) {
        return Err(refused());
    }
    let root =
        crate::preview::prepare_private_dir(&state.data_dir, crate::preview::EXTERNAL_DIFF_DIR)?;
    sweep_stale(&root, STALE_AFTER);
    let dest = new_stage_dir(&root)?.join(&name);
    let (left, right) = match side {
        DiffSideKind::Left => (Some(dest.clone()), None),
        DiffSideKind::Right => (None, Some(dest.clone())),
    };
    let sides = export_target(state, &target, left, right).await?;
    let exists = match side {
        DiffSideKind::Left => sides.left.is_some(),
        DiffSideKind::Right => sides.right.is_some(),
    };
    if !exists {
        return Err(crate::errors::invalid_argument(
            "The file does not exist in this version (it was added or deleted).",
        )
        .with_path(target_path(&target)));
    }
    make_read_only(&dest)?;
    if !default_app_allowed(&dest) {
        return Err(refused());
    }
    state.opener.open_default(&dest)
}

/// Writes both sides of `target` as read-only `left.<ext>` / `right.<ext>` in a new folder of the
/// private external-diff directory, streamed and byte for byte (binary files too). A missing side
/// (added or deleted file) is an empty file, as for the text copies.
pub(crate) async fn stage_raw_sides(
    state: &AppState,
    target: &DiffTarget,
) -> Result<(PathBuf, PathBuf), AppError> {
    use crate::programs::external_diff::{
        STALE_AFTER, make_read_only, new_stage_dir, staged_names, sweep_stale,
    };
    let root =
        crate::preview::prepare_private_dir(&state.data_dir, crate::preview::EXTERNAL_DIFF_DIR)?;
    sweep_stale(&root, STALE_AFTER);
    let dir = new_stage_dir(&root)?;
    let (ln, rn) = staged_names(&target_path(target));
    let (l, r) = (dir.join(ln), dir.join(rn));
    export_target(state, target, Some(l.clone()), Some(r.clone())).await?;
    for side in [&l, &r] {
        if !side.exists() {
            std::fs::write(side, b"").map_err(|e| {
                AppError::internal("Could not prepare the files for the external diff tool.")
                    .with_detail(e.to_string())
            })?;
        }
        make_read_only(side)?;
    }
    Ok((l, r))
}
