//! See the module docs in `mod.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use opensvn_ipc::{
    AppError, AuthMethodKind, AuthPrompt, AuthPromptKind, AuthPromptReason, AuthResponse,
    CertDecision as IpcDecision, CertFailureInfo, CertSummary, ErrorCode,
};
use secrecy::{ExposeSecret, SecretString};
use secure_store::{CredentialStore, StoreError};
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertFailure, CertInfo, CredentialRequest, Credentials,
    PromptReason,
};
use svn_types::OriginKey;
use tokio::sync::{oneshot, watch};

use super::trust::{AppDbTrustStore, hex};
use crate::db::{AccountRecord, AppDb};
use crate::ops::AppEvents;

/// How long a prompt waits for the user before it counts as cancelled.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Credential store used when the OS has none (for example no Secret Service on Linux): nothing
/// is stored and saving fails, so passwords never land in a plain file.
pub struct UnavailableCredentialStore {
    reason: String,
}

impl UnavailableCredentialStore {
    /// Store that reports `reason` when asked to save.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl CredentialStore for UnavailableCredentialStore {
    fn get(&self, _: &OriginKey, _: &str) -> secure_store::Result<Option<SecretString>> {
        Ok(None)
    }
    fn set(&self, _: &OriginKey, _: &str, _: &SecretString) -> secure_store::Result<()> {
        Err(StoreError::Unavailable(self.reason.clone()))
    }
    fn delete(&self, _: &OriginKey, _: &str) -> secure_store::Result<bool> {
        Ok(false)
    }
}

/// The OS credential store for repository passwords, or [`UnavailableCredentialStore`] when none
/// is reachable. There is no file or memory fallback.
pub fn os_credential_store() -> Arc<dyn CredentialStore> {
    match secure_store::KeyringStore::os_default() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!(error = %e, "no OS credential store; repository passwords are not saved");
            Arc::new(UnavailableCredentialStore::new(e.to_string()))
        }
    }
}

struct Pending {
    reply: oneshot::Sender<AuthResponse>,
    wants_certificate: bool,
}

/// Bridges the transport's [`AuthPrompter`] to the UI and to the saved credentials.
///
/// * Saved passwords live in the OS credential store, account metadata (user name, method) in
///   `app.sqlite`; [`AuthPrompter::stored_credentials`] combines both without showing UI.
/// * Permanently trusted certificates are served to the transport by [`AppDbTrustStore`].
/// * A saved password the server rejected (401, reported through
///   [`AuthPrompter::stored_credentials_rejected`]) is not offered again in this process until
///   the user saves new credentials for that origin, so background work and rebuilt transports
///   cannot lock the account by retrying it.
pub struct AuthBroker {
    events: Arc<dyn AppEvents>,
    db: AppDb,
    secrets: Arc<dyn CredentialStore>,
    trust: Arc<AppDbTrustStore>,
    timeout: Duration,
    pending: Mutex<HashMap<String, Pending>>,
    /// Origins whose saved password the server rejected.
    stored_rejected: Mutex<HashSet<OriginKey>>,
    /// Bumped whenever cached sessions must be dropped (sign-out, certificate removed).
    invalidations: watch::Sender<u64>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn store_error(e: &StoreError) -> AppError {
    AppError::internal("The system credential store could not be used.").with_detail(e.to_string())
}

fn failure_info(f: &CertFailure) -> CertFailureInfo {
    match f {
        CertFailure::UnknownIssuer => CertFailureInfo::UnknownIssuer,
        CertFailure::SelfSigned => CertFailureInfo::SelfSigned,
        CertFailure::Expired => CertFailureInfo::Expired,
        CertFailure::NotYetValid => CertFailureInfo::NotYetValid,
        CertFailure::HostnameMismatch => CertFailureInfo::HostnameMismatch,
        CertFailure::Revoked => CertFailureInfo::Revoked,
        CertFailure::Changed {
            old_sha256,
            new_sha256,
        } => CertFailureInfo::Changed {
            old_sha256: hex(old_sha256),
            new_sha256: hex(new_sha256),
        },
        CertFailure::Other(m) => CertFailureInfo::Other(m.clone()),
        // `CertFailure` is non-exhaustive: a new reason is shown as an unknown one.
        _ => CertFailureInfo::Other("unknown".to_owned()),
    }
}

fn method_kind(m: AuthMethod) -> AuthMethodKind {
    match m {
        AuthMethod::Negotiate => AuthMethodKind::Negotiate,
        _ => AuthMethodKind::Basic,
    }
}

fn map_reason(r: PromptReason) -> AuthPromptReason {
    match r {
        PromptReason::Rejected => AuthPromptReason::Rejected,
        PromptReason::NegotiateFailed => AuthPromptReason::NegotiateFailed,
        _ => AuthPromptReason::Initial,
    }
}

impl AuthBroker {
    /// Broker emitting prompts through `events`, persisting metadata in `db` and passwords in
    /// `secrets`.
    pub fn new(events: Arc<dyn AppEvents>, db: AppDb, secrets: Arc<dyn CredentialStore>) -> Self {
        Self::with_timeout(events, db, secrets, PROMPT_TIMEOUT)
    }

    /// Like [`AuthBroker::new`] with a custom prompt timeout.
    pub fn with_timeout(
        events: Arc<dyn AppEvents>,
        db: AppDb,
        secrets: Arc<dyn CredentialStore>,
        timeout: Duration,
    ) -> Self {
        let trust = AppDbTrustStore::load(db.clone()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not load trusted certificates");
            AppDbTrustStore::empty(db.clone())
        });
        Self {
            events,
            db,
            secrets,
            trust: Arc::new(trust),
            timeout,
            pending: Mutex::new(HashMap::new()),
            stored_rejected: Mutex::default(),
            invalidations: watch::Sender::new(0),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        locked(&self.pending)
    }

    /// Trusted certificates for the transport (loaded at start-up, kept in memory).
    #[cfg(any(test, feature = "real-engine"))]
    pub fn trust_store(&self) -> Arc<AppDbTrustStore> {
        Arc::clone(&self.trust)
    }

    /// Observes events after which cached repository sessions must be dropped: a sign-out or a
    /// removed certificate. The value only ever increases.
    #[cfg(any(test, feature = "real-engine"))]
    pub fn invalidations(&self) -> watch::Receiver<u64> {
        self.invalidations.subscribe()
    }

    fn invalidate(&self) {
        self.invalidations.send_modify(|n| *n += 1);
    }

    /// Records that the server rejected the saved password of `origin`: it is not offered again
    /// (by any transport, including ones created later) until the user saves new credentials
    /// for the origin.
    fn withhold_stored(&self, origin: &OriginKey) {
        if locked(&self.stored_rejected).insert(origin.clone()) {
            tracing::info!(host = %origin.host, "saved password was rejected; not offering it again");
        }
    }

    /// Stops trusting a certificate (row and in-memory pin) and drops cached sessions, so the
    /// next connection asks again. Returns whether it was trusted.
    pub fn forget_certificate(&self, key: &opensvn_ipc::TrustedCertKey) -> Result<bool, AppError> {
        let removed = self.db.certs_remove(key)?;
        self.trust.forget(&key.host, key.port, &key.sha256);
        if removed {
            self.invalidate();
        }
        Ok(removed)
    }

    /// Number of prompts waiting for an answer.
    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.pending().len()
    }

    /// Delivers the user's answer. The answer type must match the prompt (credentials for a
    /// credentials prompt, a decision for a certificate prompt); `Cancel` fits both.
    pub fn respond(&self, request_id: &str, response: AuthResponse) -> Result<(), AppError> {
        let mut pending = self.pending();
        let Some(entry) = pending.get(request_id) else {
            let mut e = AppError::internal("This sign-in request is no longer waiting.")
                .with_detail(request_id.to_owned());
            e.code = ErrorCode::Internal;
            return Err(e);
        };
        let fits = match &response {
            AuthResponse::Cancel => true,
            AuthResponse::Credentials { .. } | AuthResponse::Negotiate => !entry.wants_certificate,
            AuthResponse::Certificate { .. } => entry.wants_certificate,
        };
        if !fits {
            return Err(AppError::internal("The answer does not match the request."));
        }
        if let Some(entry) = pending.remove(request_id) {
            // The waiter may have timed out meanwhile; its absence is not an error for the UI.
            let _ = entry.reply.send(response);
        }
        Ok(())
    }

    /// Signs out of a realm: removes account metadata and stored secrets, and drops cached
    /// sessions (which still hold the credentials) so the next connection asks again.
    pub fn sign_out(&self, realm: &str) -> Result<(), AppError> {
        let removed = self.db.account_delete_realm(realm)?;
        self.invalidate();
        for account in removed {
            let origin = OriginKey {
                scheme: account.scheme,
                host: account.host,
                port: account.port,
                realm: realm.to_owned(),
            };
            locked(&self.stored_rejected).remove(&origin);
            if let Some(user) = account.username {
                self.secrets
                    .delete(&origin, &user)
                    .map_err(|e| store_error(&e))?;
            }
        }
        Ok(())
    }

    /// Saves `password` for `origin` in the credential store (off the async runtime: OS stores
    /// may block on IPC). Returns whether it was stored.
    async fn save_secret(&self, origin: &OriginKey, user: &str, password: &SecretString) -> bool {
        let (store, origin, user) = (Arc::clone(&self.secrets), origin.clone(), user.to_owned());
        let password = SecretString::from(password.expose_secret().to_owned());
        match tokio::task::spawn_blocking(move || store.set(&origin, &user, &password)).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not save the password");
                false
            }
            Err(e) => {
                tracing::warn!(error = %e, "credential store worker failed");
                false
            }
        }
    }

    /// Emits `prompt` and waits for the answer, the timeout or a drop.
    async fn ask(&self, prompt: AuthPrompt, wants_certificate: bool) -> Option<AuthResponse> {
        let (tx, rx) = oneshot::channel();
        let id = prompt.request_id.clone();
        self.pending().insert(
            id.clone(),
            Pending {
                reply: tx,
                wants_certificate,
            },
        );
        self.events.auth_prompt(&prompt);
        let answer = tokio::time::timeout(self.timeout, rx).await;
        self.pending().remove(&id);
        match answer {
            Ok(Ok(r)) => Some(r),
            Ok(Err(_)) => None,
            Err(_) => {
                tracing::info!("authentication prompt timed out");
                None
            }
        }
    }
}

/// Maps the webview's certificate decision onto the transport's. Every combination the transport
/// would refuse ([`CertDecision::is_valid_for`]) becomes `Reject` here already, so nothing is
/// stored for it: `AcceptOnce`/`AcceptAlways` only for an unknown issuer or self-signed
/// certificate, `ReplacePin` only for a changed one, and only `Reject` for any other failure.
pub(crate) fn map_cert_decision(decision: IpcDecision, failure: &CertFailure) -> CertDecision {
    let mapped = match decision {
        IpcDecision::Reject => CertDecision::Reject,
        IpcDecision::AcceptOnce => CertDecision::AcceptOnce,
        IpcDecision::AcceptAlways => CertDecision::AcceptAlways,
        IpcDecision::ReplacePin => CertDecision::ReplacePin,
    };
    if mapped.is_valid_for(failure) {
        mapped
    } else {
        tracing::warn!(
            ?decision,
            "certificate decision does not fit the failure; rejected"
        );
        CertDecision::Reject
    }
}

fn origin_url(o: &OriginKey) -> String {
    format!("{}://{}:{}", o.scheme, o.host, o.port)
}

#[async_trait]
impl AuthPrompter for AuthBroker {
    async fn credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        let prompt = AuthPrompt {
            request_id: crate::ops::new_op_id().0,
            kind: AuthPromptKind::Credentials,
            realm: req.origin.realm.clone(),
            url: origin_url(&req.origin),
            cert: None,
            reason: Some(map_reason(req.reason)),
            attempt: req.attempt,
            offered: req.offered.iter().copied().map(method_kind).collect(),
        };
        let (username, password, save) = match self.ask(prompt, false).await? {
            AuthResponse::Credentials {
                username,
                password,
                save,
            } => (username, password, save),
            // Single sign-on carries no secret; only honored when the server offered it.
            AuthResponse::Negotiate if req.offered.contains(&AuthMethod::Negotiate) => {
                return Some(Credentials {
                    username: String::new(),
                    password: SecretString::from(String::new()),
                    save: false,
                    method: AuthMethod::Negotiate,
                });
            }
            _ => return None,
        };
        let method = if req.offered.contains(&AuthMethod::Basic) || req.offered.is_empty() {
            AuthMethod::Basic
        } else {
            req.offered[0]
        };
        let password = SecretString::from(password.expose().to_owned());
        if save && method == AuthMethod::Basic {
            let recorded = self.db.account_upsert(&AccountRecord {
                scheme: &req.origin.scheme,
                host: &req.origin.host,
                port: req.origin.port,
                realm: &req.origin.realm,
                username: &username,
                negotiate: false,
            });
            match recorded {
                Ok(()) => {
                    if self.save_secret(&req.origin, &username, &password).await {
                        // The user replaced the saved password: it may be offered again.
                        locked(&self.stored_rejected).remove(&req.origin);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "could not record account metadata"),
            }
        }
        Some(Credentials {
            username,
            password,
            save,
            method,
        })
    }

    async fn stored_credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        if !req.offered.is_empty() && !req.offered.contains(&AuthMethod::Basic) {
            return None;
        }
        let origin = &req.origin;
        if locked(&self.stored_rejected).contains(origin) {
            return None;
        }
        let (username, negotiate) =
            match self
                .db
                .account_get(&origin.scheme, &origin.host, origin.port, &origin.realm)
            {
                Ok(Some(account)) => account,
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!(error = %e, "could not read account metadata");
                    return None;
                }
            };
        let username = username.filter(|u| !u.is_empty() && !negotiate)?;
        let (store, o, u) = (Arc::clone(&self.secrets), origin.clone(), username.clone());
        let password = match tokio::task::spawn_blocking(move || store.get(&o, &u)).await {
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return None,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not read the saved password");
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "credential store worker failed");
                return None;
            }
        };
        Some(Credentials {
            username,
            password,
            save: true,
            method: AuthMethod::Basic,
        })
    }

    async fn stored_credentials_rejected(&self, req: &CredentialRequest) {
        // In-memory only: the transport does not race this call against cancellation.
        self.withhold_stored(&req.origin);
    }

    async fn confirm_certificate(&self, info: &CertInfo) -> CertDecision {
        let fingerprint = hex(&info.sha256);
        let permanent_ok = matches!(
            info.failure,
            CertFailure::UnknownIssuer | CertFailure::SelfSigned
        );
        if permanent_ok
            && self
                .db
                .cert_is_trusted(&info.host, info.port, &fingerprint)
                .unwrap_or(false)
        {
            return CertDecision::AcceptAlways;
        }
        let prompt = AuthPrompt {
            request_id: crate::ops::new_op_id().0,
            kind: AuthPromptKind::Certificate,
            realm: String::new(),
            url: format!("https://{}:{}", info.host, info.port),
            cert: Some(CertSummary {
                host: format!("{}:{}", info.host, info.port),
                subject: info.subject.clone(),
                issuer: info.issuer.clone(),
                not_before: opensvn_ipc::time_fmt::timestamp_to_iso(info.not_before),
                not_after: opensvn_ipc::time_fmt::timestamp_to_iso(info.not_after),
                sha256: fingerprint.clone(),
                failure: failure_info(&info.failure),
            }),
            reason: None,
            attempt: 0,
            offered: Vec::new(),
        };
        let Some(AuthResponse::Certificate { decision }) = self.ask(prompt, true).await else {
            return CertDecision::Reject;
        };
        // The UI is not a trust boundary: the decision must fit the actual failure.
        let decision = map_cert_decision(decision, &info.failure);
        let expires = opensvn_ipc::time_fmt::timestamp_to_iso(info.not_after);
        match decision {
            CertDecision::AcceptAlways => {
                if let Err(e) =
                    self.db
                        .cert_trust(&info.host, info.port, &fingerprint, &info.subject, &expires)
                {
                    tracing::warn!(error = %e, "could not store trusted certificate");
                    return CertDecision::AcceptOnce;
                }
                CertDecision::AcceptAlways
            }
            CertDecision::ReplacePin => {
                if let Err(e) = self.db.cert_replace_pin(
                    &info.host,
                    info.port,
                    &fingerprint,
                    &info.subject,
                    &expires,
                ) {
                    tracing::warn!(error = %e, "could not replace the pinned certificate");
                    return CertDecision::Reject;
                }
                // The transport replaces its own pin on `ReplacePin` too.
                CertDecision::ReplacePin
            }
            other => other,
        }
    }
}
