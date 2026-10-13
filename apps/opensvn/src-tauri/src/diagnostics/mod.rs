//! Diagnostics bundle: built in memory, shown to the user for review, saved only after the user
//! confirms and picks the destination in a native dialog. Nothing is uploaded.

mod bundle;
mod redact;
pub mod zip;

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use opensvn_ipc::AppError;
use serde::{Deserialize, Serialize};
use specta::Type;

pub use bundle::{BundleInputs, build_bundle};
use zip::write_zip;

use crate::errors::invalid_argument;

/// What to hide besides secrets (which are always removed).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsOptions {
    /// Replace the home folder and working copy paths with placeholders.
    pub hide_paths: bool,
    /// Remove commit messages from log lines.
    pub hide_commit_messages: bool,
}

/// One file of the bundle as the user sees it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsEntry {
    /// Path inside the zip.
    pub name: String,
    /// Size in bytes.
    pub size: u32,
    /// Full text that will be saved.
    pub content: String,
}

/// The bundle awaiting the user's decision.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsPreview {
    /// Pass to `diagnostics_export` to save exactly these contents.
    pub token: String,
    /// Files in the bundle.
    pub entries: Vec<DiagnosticsEntry>,
}

/// A previewed bundle: its token and files.
type Pending = (String, Vec<(String, String)>);

/// Holds the previewed bundle until it is exported or replaced.
#[derive(Default)]
pub struct DiagnosticsService {
    pending: Mutex<Option<Pending>>,
}

impl DiagnosticsService {
    /// Builds a bundle and keeps it for `export` (replacing an earlier preview).
    pub fn preview(
        &self,
        inputs: &BundleInputs,
        options: DiagnosticsOptions,
    ) -> DiagnosticsPreview {
        let files = build_bundle(inputs, options);
        let token = crate::ops::new_op_id().0;
        let entries = files
            .iter()
            .map(|(name, content)| DiagnosticsEntry {
                name: name.clone(),
                size: u32::try_from(content.len()).unwrap_or(u32::MAX),
                content: content.clone(),
            })
            .collect();
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some((token.clone(), files));
        DiagnosticsPreview { token, entries }
    }

    /// Writes the previewed bundle to `dest` (a path from a native dialog). The preview is
    /// consumed: exporting the same token twice is refused.
    pub fn export(&self, token: &str, dest: &Path) -> Result<(), AppError> {
        let taken = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some((pending_token, files)) = taken else {
            return Err(invalid_argument("Preview the diagnostics first."));
        };
        if pending_token != token {
            // A stale token must not consume the newer preview.
            *self.pending.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((pending_token, files));
            return Err(invalid_argument("This preview is outdated. Preview again."));
        }
        let entries: Vec<(String, Vec<u8>)> = files
            .into_iter()
            .map(|(n, c)| (n, c.into_bytes()))
            .collect();
        let bytes = write_zip(&entries).map_err(|e| {
            AppError::internal("Could not create the diagnostics file.")
                .with_detail(format!("{e:?}"))
        })?;
        std::fs::write(dest, bytes).map_err(|e| {
            AppError::internal("Could not save the diagnostics file.")
                .with_detail(e.to_string())
                .with_path(dest.to_string_lossy())
        })
    }
}

#[cfg(test)]
mod tests;
