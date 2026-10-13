//! App self-update from a private GitHub releases repository.
//!
//! * The check is user initiated (`update_check`); nothing runs on a timer here.
//! * The GitHub read token lives only in the OS credential store (macOS Keychain, Windows
//!   Credential Manager, Linux Secret Service) under [`TOKEN_KEY`]. It is write-only from the UI:
//!   it can be set or cleared, and only its presence can be read back. When no OS store is
//!   reachable the token cannot be saved at all; it is never kept in a file, the settings or
//!   memory as a fallback.
//! * Every URL the token is sent to is validated first: each update endpoint from the
//!   `plugins.updater.endpoints` config before the check, and the download URL announced by the
//!   update manifest before the download. Both must be `https`, host exactly `api.github.com`,
//!   default port, no userinfo, path inside the expected repository.
//! * Redirects are followed only over `https` to GitHub's API and release asset hosts, and the
//!   `Authorization` header is marked sensitive. Any updater `dangerous*` TLS option in the config
//!   fails the build (`build.rs`) and is refused at runtime too.
//! * `install` re-reads the token: if it was cleared after the check, none is sent.

use std::sync::Arc;
#[cfg(test)]
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

/// Whether `url` is the GitHub REST API under `/repos/{repo}/`: `https`, host exactly
/// `api.github.com`, default port, no userinfo, and no dot segments or encoded separators that
/// could leave the repository path.
fn is_release_api_url(url: &Url, repo: &str) -> bool {
    let path = url.path().to_ascii_lowercase();
    url.scheme() == "https"
        && url.host_str() == Some("api.github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().starts_with(&format!("/repos/{repo}/"))
        && !url.path().split('/').any(|s| s == ".." || s == ".")
        && !path.contains("%2f")
        && !path.contains("%2e")
        && !path.contains("%5c")
}

/// Rejects any update download URL that is not the GitHub API for the expected repository.
pub fn validate_download_url(url: &Url, repo: &str) -> Result<(), AppError> {
    if is_release_api_url(url, repo) {
        Ok(())
    } else {
        tracing::warn!(host = ?url.host_str(), "update download URL rejected");
        Err(invalid_argument(
            "The update is not served from the expected release location.",
        ))
    }
}

/// Hosts a redirect from the release API may lead to (release asset storage).
const REDIRECT_HOSTS: &[&str] = &[
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
/// Most redirects followed for one request.
const MAX_REDIRECTS: usize = 5;

/// Whether the updater may follow a redirect to `url` after `hops` earlier redirects: `https`,
/// default port, no userinfo, one of [`REDIRECT_HOSTS`].
pub fn redirect_allowed(url: &Url, hops: usize) -> bool {
    hops < MAX_REDIRECTS
        && url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|h| REDIRECT_HOSTS.contains(&h))
}

/// Request headers for the updater: with a token, `Authorization` (marked sensitive so it never
/// shows in a debug print) and the binary `Accept` GitHub asset downloads need.
pub fn request_headers(
    token: Option<&SecretString>,
) -> Result<reqwest::header::HeaderMap, AppError> {
    use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
    let mut h = HeaderMap::new();
    if let Some(t) = token {
        let mut v = HeaderValue::try_from(format!("Bearer {}", t.expose_secret()))
            .map_err(|_| invalid_argument("The update token contains invalid characters."))?;
        v.set_sensitive(true);
        h.insert(AUTHORIZATION, v);
        h.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
    }
    Ok(h)
}

/// The updater `dangerous*` options set to anything but `false` in `config`.
fn dangerous_flags(config: Option<&serde_json::Value>) -> Vec<String> {
    config
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(k, v)| k.starts_with("dangerous") && **v != serde_json::Value::Bool(false))
        .map(|(k, _)| k.clone())
        .collect()
}

/// Reads and validates `plugins.updater.endpoints` (`config` is the `plugins.updater` object).
/// Every endpoint must pass the same check as the download URL, so the token is only ever sent to
/// `api.github.com` for the release repository. A missing, empty or invalid list is refused.
pub fn configured_endpoints(
    config: Option<&serde_json::Value>,
    repo: &str,
) -> Result<Vec<Url>, AppError> {
    let not_configured = || invalid_argument("Updates are not configured in this build.");
    let dangerous = dangerous_flags(config);
    if !dangerous.is_empty() {
        tracing::warn!(?dangerous, "updater TLS options relaxed; updates disabled");
        return Err(invalid_argument(
            "Updates are disabled because this build relaxes update security.",
        ));
    }
    let list = config
        .and_then(|c| c.get("endpoints"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(not_configured)?;
    if list.is_empty() {
        return Err(not_configured());
    }
    list.iter()
        .map(|v| {
            let url = v
                .as_str()
                .and_then(|s| Url::parse(s).ok())
                .ok_or_else(not_configured)?;
            if is_release_api_url(&url, repo) {
                Ok(url)
            } else {
                tracing::warn!(host = ?url.host_str(), "update endpoint rejected");
                Err(invalid_argument(
                    "The update endpoint is not the expected release location.",
                ))
            }
        })
        .collect()
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

/// In-memory secrets for tests. Never used by the app: the token belongs in the OS store only.
#[cfg(test)]
#[derive(Default)]
pub struct MemorySecretStore {
    items: Mutex<std::collections::HashMap<String, SecretString>>,
}

#[cfg(test)]
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

/// Address of an app secret inside the OS credential store: a pseudo origin that no repository
/// URL can produce (scheme `opensvn-app`, port 0, a realm starting with U+0001).
fn app_secret_origin() -> svn_types::OriginKey {
    svn_types::OriginKey {
        scheme: "opensvn-app".into(),
        host: "app".into(),
        port: 0,
        realm: "\u{1}app-secrets".into(),
    }
}

fn store_error(e: &secure_store::StoreError) -> AppError {
    AppError::internal("The system credential store could not be used.").with_detail(e.to_string())
}

/// [`SecretStore`] over the OS credential store (`secure-store`).
pub struct KeychainSecretStore {
    store: Arc<dyn secure_store::CredentialStore>,
}

impl KeychainSecretStore {
    /// Wraps a credential store (the OS one in the app, a memory one in tests).
    pub fn new(store: Arc<dyn secure_store::CredentialStore>) -> Self {
        Self { store }
    }
}

impl SecretStore for KeychainSecretStore {
    fn get(&self, key: &str) -> Result<Option<SecretString>, AppError> {
        self.store
            .get(&app_secret_origin(), key)
            .map_err(|e| store_error(&e))
    }
    fn set(&self, key: &str, value: &SecretString) -> Result<(), AppError> {
        self.store
            .set(&app_secret_origin(), key, value)
            .map_err(|e| store_error(&e))
    }
    fn delete(&self, key: &str) -> Result<(), AppError> {
        self.store
            .delete(&app_secret_origin(), key)
            .map(|_| ())
            .map_err(|e| store_error(&e))
    }
}

/// Used when no OS credential store is reachable: nothing can be saved, nothing is stored.
pub struct UnavailableSecretStore {
    reason: String,
}

impl SecretStore for UnavailableSecretStore {
    fn get(&self, _: &str) -> Result<Option<SecretString>, AppError> {
        Ok(None)
    }
    fn set(&self, _: &str, _: &SecretString) -> Result<(), AppError> {
        Err(AppError::internal(
            "No system credential store is available, so the update token cannot be saved.",
        )
        .with_detail(self.reason.clone()))
    }
    fn delete(&self, _: &str) -> Result<(), AppError> {
        Ok(())
    }
}

/// The OS credential store, or [`UnavailableSecretStore`] when none is reachable (for example no
/// Secret Service daemon on Linux). There is no file or memory fallback.
pub fn os_secret_store() -> Arc<dyn SecretStore> {
    match secure_store::KeyringStore::os_default() {
        Ok(s) => Arc::new(KeychainSecretStore::new(Arc::new(s))),
        Err(e) => {
            tracing::warn!(error = %e, "no OS credential store; the update token cannot be saved");
            Arc::new(UnavailableSecretStore {
                reason: e.to_string(),
            })
        }
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
    /// Validated `plugins.updater.endpoints`, or why updates cannot run. Without an `updater`
    /// config the plugin is not registered (`lib.rs`) and the builder must not be touched.
    endpoints: Result<Vec<Url>, AppError>,
    found: tokio::sync::Mutex<Option<tauri_plugin_updater::Update>>,
}

impl<R: tauri::Runtime> TauriUpdater<R> {
    /// Wraps an app handle.
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        let endpoints = configured_endpoints(app.config().plugins.0.get("updater"), RELEASES_REPO);
        Self {
            app,
            endpoints,
            found: tokio::sync::Mutex::new(None),
        }
    }

    fn builder(
        &self,
        token: Option<&SecretString>,
    ) -> Result<tauri_plugin_updater::UpdaterBuilder, AppError> {
        // Endpoints are checked before the builder exists and before any header carries the token.
        let endpoints = self.endpoints.clone()?;
        let headers = request_headers(token)?;
        let b = self
            .app
            .updater_builder()
            .endpoints(endpoints)
            .map_err(updater_error)?
            .headers(headers)
            .configure_client(|c| {
                c.https_only(true)
                    .redirect(reqwest::redirect::Policy::custom(|attempt| {
                        if redirect_allowed(attempt.url(), attempt.previous().len()) {
                            attempt.follow()
                        } else {
                            attempt.stop()
                        }
                    }))
            });
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
        let update = self.found.lock().await.take();
        let Some(mut update) = update else {
            return Err(invalid_argument("Check for updates first."));
        };
        validate_download_url(&update.download_url, RELEASES_REPO)?;
        // `token` is read from the credential store for this call; the headers the check used are
        // dropped so a token cleared since then is not sent.
        update.headers = request_headers(token.as_ref())?;
        update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(updater_error)?;
        self.app.restart()
    }
}

#[cfg(test)]
mod tests;
