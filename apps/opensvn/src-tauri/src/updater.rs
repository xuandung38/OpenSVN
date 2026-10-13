//! App self-update from a private GitHub releases repository.
//!
//! * The check is user initiated (`update_check`); nothing runs on a timer here.
//! * The GitHub read token lives in the credential store under [`TOKEN_KEY`]. It is write-only
//!   from the UI: it can be set or cleared, and only its presence can be read back.
//! * The download URL announced by the update manifest is validated before anything is
//!   downloaded: `https`, host exactly `api.github.com`, path inside the expected repository.

use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use opensvn_ipc::AppError;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_plugin_updater::UpdaterExt;
use url::Url;

use crate::errors::invalid_argument;

/// Credential store key of the GitHub read token.
pub const TOKEN_KEY: &str = "updater/github-token";
/// Repository that publishes releases (`owner/name`).
pub const RELEASES_REPO: &str = "xuandung38/OpenSVN-releases";

/// A newer version found by a check.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// Version string.
    pub version: String,
    /// Release notes, if any.
    pub notes: Option<String>,
}

/// Rejects any update download URL that is not the GitHub API for the expected repository.
pub fn validate_download_url(url: &Url, repo: &str) -> Result<(), AppError> {
    let ok = url.scheme() == "https"
        && url.host_str() == Some("api.github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().starts_with(&format!("/repos/{repo}/"))
        // no `..` or encoded separators that could escape the repository path
        && !url.path().split('/').any(|s| s == ".." || s == ".")
        && !url.path().to_ascii_lowercase().contains("%2f")
        && !url.path().to_ascii_lowercase().contains("%2e");
    if ok {
        Ok(())
    } else {
        tracing::warn!(host = ?url.host_str(), "update download URL rejected");
        Err(invalid_argument(
            "The update is not served from the expected release location.",
        ))
    }
}

/// Credential store access for the updater token (backed by the OS credential store).
pub trait SecretStore: Send + Sync {
    /// Reads a secret.
    fn get(&self, key: &str) -> Result<Option<SecretString>, AppError>;
    /// Stores or replaces a secret.
    fn set(&self, key: &str, value: &SecretString) -> Result<(), AppError>;
    /// Removes a secret (absent is fine).
    fn delete(&self, key: &str) -> Result<(), AppError>;
}

/// In-memory secrets, used until the OS credential store crate is wired in: a token set here is
/// lost when the app exits and never touches disk.
#[derive(Default)]
pub struct MemorySecretStore {
    items: Mutex<std::collections::HashMap<String, SecretString>>,
}

impl SecretStore for MemorySecretStore {
    fn get(&self, key: &str) -> Result<Option<SecretString>, AppError> {
        Ok(self
            .items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned())
    }
    fn set(&self, key: &str, value: &SecretString) -> Result<(), AppError> {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), AppError> {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        Ok(())
    }
}

/// Performs the actual check and installation.
#[async_trait]
pub trait UpdateControl: Send + Sync + 'static {
    /// Looks for a newer version using `token` for GitHub; remembers the update for `install`.
    async fn check(&self, token: Option<SecretString>) -> Result<Option<UpdateInfo>, AppError>;
    /// Downloads and installs the update found by the last `check`, then restarts the app.
    async fn install(&self, token: Option<SecretString>) -> Result<(), AppError>;
}

/// [`UpdateControl`] over the Tauri updater plugin (must be registered, see `lib.rs`).
pub struct TauriUpdater<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    found: tokio::sync::Mutex<Option<tauri_plugin_updater::Update>>,
}

impl<R: tauri::Runtime> TauriUpdater<R> {
    /// Wraps an app handle.
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        Self {
            app,
            found: tokio::sync::Mutex::new(None),
        }
    }

    fn builder(
        &self,
        token: Option<&SecretString>,
    ) -> Result<tauri_plugin_updater::UpdaterBuilder, AppError> {
        let mut b = self.app.updater_builder();
        if let Some(t) = token {
            b = b
                .header("Authorization", format!("Bearer {}", t.expose_secret()))
                .map_err(updater_error)?
                .header("Accept", "application/octet-stream")
                .map_err(updater_error)?;
        }
        Ok(b)
    }
}

fn updater_error(e: impl std::fmt::Display) -> AppError {
    AppError::internal("The update check failed.").with_detail(e.to_string())
}

#[async_trait]
impl<R: tauri::Runtime> UpdateControl for TauriUpdater<R> {
    async fn check(&self, token: Option<SecretString>) -> Result<Option<UpdateInfo>, AppError> {
        let updater = self
            .builder(token.as_ref())?
            .build()
            .map_err(updater_error)?;
        let update = updater.check().await.map_err(updater_error)?;
        let info = update.as_ref().map(|u| UpdateInfo {
            version: u.version.clone(),
            notes: u.body.clone(),
        });
        *self.found.lock().await = update;
        Ok(info)
    }

    async fn install(&self, token: Option<SecretString>) -> Result<(), AppError> {
        let _ = token; // the token is carried by the update object built in `check`
        let update = self.found.lock().await.take();
        let Some(update) = update else {
            return Err(invalid_argument("Check for updates first."));
        };
        validate_download_url(&update.download_url, RELEASES_REPO)?;
        update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(updater_error)?;
        self.app.restart()
    }
}

#[cfg(test)]
mod tests;
