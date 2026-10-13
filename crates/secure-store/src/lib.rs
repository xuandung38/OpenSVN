//! Secret storage for OpenSVN credentials.
//!
//! Secrets live in the operating system credential store (macOS Keychain, Windows Credential
//! Manager, Linux Secret Service) through `keyring-core`. When no OS store is reachable the
//! operations fail with [`StoreError::Unavailable`]; secrets are never written to plain files.
//!
//! Entries use the service name [`SERVICE`] and an account string derived from
//! `(OriginKey, user)` by [`account_key`]. Proxy passwords use an [`OriginKey`] whose realm is
//! [`PROXY_REALM`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use secrecy::{ExposeSecret, SecretString};
use svn_types::OriginKey;

/// Service name under which every entry is stored.
pub const SERVICE: &str = "opensvn";

/// Realm used for proxy credentials so they never collide with repository realms.
pub const PROXY_REALM: &str = "\u{1}proxy";

/// Failure of a secret store operation.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No usable OS credential store (for example no Secret Service on a headless Linux box).
    #[error("no usable OS credential store: {0}")]
    Unavailable(String),
    /// The store exists but refused access (locked keychain, denied prompt).
    #[error("credential store access denied: {0}")]
    AccessDenied(String),
    /// Any other backend failure.
    #[error("credential store failure: {0}")]
    Backend(String),
}

/// Result alias for store operations.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Store of passwords addressed by `(origin, user)`.
pub trait CredentialStore: Send + Sync {
    /// Returns the password, or `None` when nothing is stored.
    fn get(&self, origin: &OriginKey, user: &str) -> Result<Option<SecretString>>;
    /// Stores or replaces the password.
    fn set(&self, origin: &OriginKey, user: &str, password: &SecretString) -> Result<()>;
    /// Removes the entry; returns whether one existed.
    fn delete(&self, origin: &OriginKey, user: &str) -> Result<bool>;
}

/// Escapes `%` and `|` so fields can be joined unambiguously.
fn escape(field: &str, out: &mut String) {
    for c in field.chars() {
        match c {
            '%' => out.push_str("%25"),
            '|' => out.push_str("%7C"),
            c => out.push(c),
        }
    }
}

/// Builds the account string for an `(origin, user)` pair: `scheme|host|port|realm|user` with
/// `%` and `|` percent-escaped. Distinct pairs always yield distinct strings.
pub fn account_key(origin: &OriginKey, user: &str) -> String {
    let mut out = String::new();
    for (i, field) in [
        origin.scheme.as_str(),
        origin.host.as_str(),
        &origin.port.to_string(),
        origin.realm.as_str(),
        user,
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            out.push('|');
        }
        escape(field, &mut out);
    }
    out
}

/// In-memory store for tests and for sessions that must not persist secrets.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, SecretString>>,
}

impl MemoryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, origin: &OriginKey, user: &str) -> Result<Option<SecretString>> {
        let map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(map.get(&account_key(origin, user)).cloned())
    }

    fn set(&self, origin: &OriginKey, user: &str, password: &SecretString) -> Result<()> {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        map.insert(account_key(origin, user), password.clone());
        Ok(())
    }

    fn delete(&self, origin: &OriginKey, user: &str) -> Result<bool> {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(map.remove(&account_key(origin, user)).is_some())
    }
}

/// Store backed by an OS credential store through `keyring-core`.
pub struct KeyringStore {
    store: Arc<keyring_core::CredentialStore>,
}

impl std::fmt::Debug for KeyringStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringStore").finish_non_exhaustive()
    }
}

impl KeyringStore {
    /// Opens the native store of the current platform.
    pub fn os_default() -> Result<Self> {
        Ok(Self {
            store: native_store()?,
        })
    }

    /// Wraps an explicit `keyring-core` store (used by tests with the mock store).
    pub fn with_store(store: Arc<keyring_core::CredentialStore>) -> Self {
        Self { store }
    }

    fn entry(&self, origin: &OriginKey, user: &str) -> Result<keyring_core::Entry> {
        self.store
            .build(SERVICE, &account_key(origin, user), None)
            .map_err(map_error)
    }
}

impl CredentialStore for KeyringStore {
    fn get(&self, origin: &OriginKey, user: &str) -> Result<Option<SecretString>> {
        match self.entry(origin, user)?.get_password() {
            Ok(p) => Ok(Some(SecretString::from(p))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(map_error(e)),
        }
    }

    fn set(&self, origin: &OriginKey, user: &str, password: &SecretString) -> Result<()> {
        self.entry(origin, user)?
            .set_password(password.expose_secret())
            .map_err(map_error)
    }

    fn delete(&self, origin: &OriginKey, user: &str) -> Result<bool> {
        match self.entry(origin, user)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(map_error(e)),
        }
    }
}

/// Maps a `keyring-core` error. Messages never include secret material.
fn map_error(err: keyring_core::Error) -> StoreError {
    use keyring_core::Error as E;
    match err {
        E::NoStorageAccess(e) => StoreError::AccessDenied(e.to_string()),
        E::NoDefaultStore | E::NotSupportedByStore(_) => StoreError::Unavailable(err.to_string()),
        other => StoreError::Backend(other.to_string()),
    }
}

/// macOS Keychain store (round trip verified locally with the ignored `os_store_roundtrip`).
#[cfg(target_os = "macos")]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>> {
    let store = apple_native_keyring_store::keychain::Store::new()
        .map_err(|e| StoreError::Unavailable(e.to_string()))?;
    Ok(store)
}

/// Windows Credential Manager store.
///
/// Unverified: compiled and exercised only by CI/manual runs on Windows (follow-up for P13).
#[cfg(target_os = "windows")]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>> {
    let store = windows_native_keyring_store::Store::new()
        .map_err(|e| StoreError::Unavailable(e.to_string()))?;
    Ok(store)
}

/// Secret Service (zbus) store; fails with a clear error when no keyring daemon is running and
/// never falls back to plaintext.
///
/// Unverified: not run on Linux yet; needs a headless gnome-keyring session (follow-up for P13).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>> {
    let store = zbus_secret_service_keyring_store::Store::new().map_err(|e| {
        StoreError::Unavailable(format!(
            "Secret Service not reachable (is a keyring daemon running?): {e}"
        ))
    })?;
    Ok(store)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "freebsd"
)))]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>> {
    Err(StoreError::Unavailable(
        "no credential store for this platform".to_owned(),
    ))
}
