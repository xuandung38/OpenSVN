//! Diffs of Word and Excel files as text, like TortoiseSVN's `diff-doc` / `diff-xls` scripts but
//! without Office: both versions are written to a private folder by the engine (streamed, any
//! size), the text parts are read out of each package (`office-text`), and the two texts go
//! through the usual text diff (display budget, whitespace option, large-text banner). The
//! result says it compares extracted text, not formatting (`FileDiff.office`).
//!
//! Legacy 97-2003 files, protected and damaged documents are reported with
//! `office.extracted = false` and the reason, as a binary diff, so the UI can offer to open both
//! versions or the external diff tool instead.

use std::path::Path;
use std::time::Duration;

use office_text::{ExtractError, Limits, OfficeFormat, extract_text};
use opensvn_ipc::{AppError, DiffTarget, FileDiff, OfficeKind, OfficeText, PropChange};

use crate::app_state::AppState;
use crate::diff_sides::{export_target, target_path};
use crate::engine::{DiffOptions, OpContext};

/// Extracted text larger than this is sent only on request (`force_large`), like file text.
const INLINE_LIMIT: usize = 5 * 1024 * 1024;
/// Folders left behind by an interrupted extraction are removed after this long.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// Office family of a path, by extension.
pub(crate) fn office_format(path: &str) -> Option<OfficeFormat> {
    OfficeFormat::from_path(path)
}

fn kind_of(format: OfficeFormat) -> OfficeKind {
    match format {
        OfficeFormat::Docx | OfficeFormat::LegacyDoc => OfficeKind::Word,
        OfficeFormat::Xlsx | OfficeFormat::LegacyXls => OfficeKind::Excel,
    }
}

fn sideless(path: &str, prop_changes: Vec<PropChange>, office: OfficeText) -> FileDiff {
    FileDiff {
        path: path.to_owned(),
        binary: false,
        mime: None,
        left: None,
        right: None,
        hunks: None,
        prop_changes,
        image: None,
        too_large: None,
        too_complex: None,
        office: Some(office),
    }
}

/// A binary diff explaining why no text could be extracted.
fn not_extracted(
    path: &str,
    kind: OfficeKind,
    err: &ExtractError,
    props: Vec<PropChange>,
) -> FileDiff {
    FileDiff {
        binary: true,
        mime: Some("application/octet-stream".to_owned()),
        ..sideless(
            path,
            props,
            OfficeText {
                kind,
                extracted: false,
                reason: Some(err.to_string()),
            },
        )
    }
}

/// Text of the Office file at `file`.
fn extract_file(file: &Path, format: OfficeFormat) -> Result<String, ExtractError> {
    let f = std::fs::File::open(file).map_err(|e| ExtractError::Io(e.to_string()))?;
    extract_text(std::io::BufReader::new(f), format, &Limits::default())
}

/// Property rows of a working copy diff (the regular diff computes them); none for revision
/// diffs, whose engines report none either.
async fn wc_prop_changes(state: &AppState, target: &DiffTarget) -> Vec<PropChange> {
    if !matches!(target, DiffTarget::Wc { .. } | DiffTarget::WcVsRev { .. }) {
        return Vec::new();
    }
    crate::commands::fetch_diff(state, target.clone(), DiffOptions::default())
        .await
        .map(|d| d.prop_changes)
        .unwrap_or_default()
}

/// The text diff of `target` when it is a Word or Excel file (by extension), else `None`.
pub(crate) async fn office_diff(
    state: &AppState,
    target: &DiffTarget,
    opts: DiffOptions,
) -> Result<Option<FileDiff>, AppError> {
    let path = target_path(target);
    let Some(format) = office_format(&path) else {
        return Ok(None);
    };
    let kind = kind_of(format);
    let props = wc_prop_changes(state, target).await;
    if !format.is_supported() {
        let diff = not_extracted(&path, kind, &ExtractError::Legacy(format), props);
        return Ok(Some(diff));
    }
    let root =
        crate::preview::prepare_private_dir(&state.data_dir, crate::preview::OFFICE_DIFF_DIR)?;
    crate::programs::external_diff::sweep_stale(&root, STALE_AFTER);
    let dir = root.join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir(&dir).map_err(|e| {
        AppError::internal("Could not prepare the Office files for comparison.")
            .with_detail(e.to_string())
    })?;
    let outcome = diff_in(state, target, &dir, &path, format, kind, opts, props).await;
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!(error = %e, "could not remove the Office diff folder");
    }
    outcome.map(Some)
}

#[allow(clippy::too_many_arguments)]
async fn diff_in(
    state: &AppState,
    target: &DiffTarget,
    dir: &Path,
    path: &str,
    format: OfficeFormat,
    kind: OfficeKind,
    opts: DiffOptions,
    props: Vec<PropChange>,
) -> Result<FileDiff, AppError> {
    let (l, r) = (dir.join("left"), dir.join("right"));
    let sides = export_target(state, target, Some(l.clone()), Some(r.clone())).await?;
    let (has_left, has_right) = (sides.left.is_some(), sides.right.is_some());
    let texts = tokio::task::spawn_blocking(move || {
        let left = has_left.then(|| extract_file(&l, format)).transpose()?;
        let right = has_right.then(|| extract_file(&r, format)).transpose()?;
        Ok::<_, ExtractError>((left, right))
    })
    .await
    .map_err(|e| {
        AppError::internal("The Office text extraction failed.").with_detail(e.to_string())
    })?;
    let (left, right) = match texts {
        Ok(t) => t,
        Err(e) => return Ok(not_extracted(path, kind, &e, props)),
    };
    let office = OfficeText {
        kind,
        extracted: true,
        reason: None,
    };
    let too_large = [&left, &right]
        .iter()
        .any(|t| t.as_ref().is_some_and(|t| t.len() > INLINE_LIMIT));
    if too_large && !opts.force_large {
        return Ok(FileDiff {
            too_large: Some(true),
            ..sideless(path, props, office)
        });
    }
    let side = |text: Option<String>, label: Option<String>| {
        text.zip(label).map(|(t, l)| (t.into_bytes(), l))
    };
    let mut diff = crate::engine::text::file_diff(
        path,
        side(left, sides.left),
        side(right, sides.right),
        opts,
        &OpContext::detached(),
    )
    .map_err(|e| AppError::from(&e))?;
    diff.prop_changes = props;
    diff.office = Some(office);
    Ok(diff)
}
