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
use crate::db::{AccountRecord, AppDb, SavedAccount};
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
/// * A password typed at the prompt with "save" is persisted only once the server accepted it
///   ([`AuthPrompter::credentials_accepted`]), so a mistyped password never lands in the store.
/// * A saved password the server rejected (401, reported through
///   [`AuthPrompter::stored_credentials_rejected`]) is marked stale in `app.sqlite` and not
///   offered again, in this process or after a restart, until the user saves a password for
///   that account again. It stays in the OS store (a 401 is not always a wrong password); the
///   user is prompted instead, and background work fails with `auth-required` without sending
///   it. This keeps background work and rebuilt transports from locking a directory account.
pub struct AuthBroker {
    events: Arc<dyn AppEvents>,
    db: AppDb,
    secrets: Arc<dyn CredentialStore>,
    trust: Arc<AppDbTrustStore>,
    timeout: Duration,
    pending: Mutex<HashMap<String, Pending>>,
    /// Origins whose saved password the server rejected (also kept in memory in case the
    /// database cannot be written).
    stored_rejected: Mutex<HashSet<OriginKey>>,
    /// User name of the saved password last offered per origin, to mark the right account stale.
    offered: Mutex<HashMap<OriginKey, String>>,
    /// Origins with a typed answer the server has neither accepted nor rejected yet.
    awaiting: Mutex<HashSet<OriginKey>>,
    /// Origins whose last typed answer the server rejected; the next prompt says so.
    typed_rejected: Mutex<HashSet<OriginKey>>,
    /// Bumped whenever cached sessions must be dropped (sign-out, certificate removed).
    invalidations: watch::Sender<u64>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
            offered: Mutex::default(),
            awaiting: Mutex::default(),
            typed_rejected: Mutex::default(),
            invalidations: watch::Sender::new(0),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        locked(&self.pending)
    }

    /// Trusted certificates for the transport (loaded at start-up, kept in memory).
    #[allow(dead_code)] // the real engine's connector (built from `lib.rs` from WP7 on)
    pub fn trust_store(&self) -> Arc<AppDbTrustStore> {
        Arc::clone(&self.trust)
    }

    /// Observes events after which cached repository sessions must be dropped: a sign-out or a
    /// removed certificate. The value only ever increases.
    #[allow(dead_code)] // the real engine's connector (built from `lib.rs` from WP7 on)
    pub fn invalidations(&self) -> watch::Receiver<u64> {
        self.invalidations.subscribe()
    }

    fn invalidate(&self) {
        self.invalidations.send_modify(|n| *n += 1);
    }

    /// Records that the server rejected the saved password of `origin`: it is not offered again
    /// (by any transport, including ones created later, and after a restart) until the user
    /// saves a password for the origin. The password itself stays in the store.
    fn withhold_stored(&self, origin: &OriginKey) {
        if locked(&self.stored_rejected).insert(origin.clone()) {
            tracing::info!(host = %origin.host, "saved password was rejected; not offering it again");
        }
        let username = locked(&self.offered).remove(origin).or_else(|| {
            self.account(origin)
                .and_then(|a| a.username)
                .filter(|u| !u.is_empty())
        });
        if let Some(user) = username
            && let Err(e) = self.db.account_mark_stale(
                &origin.scheme,
                &origin.host,
                origin.port,
                &origin.realm,
                &user,
            )
        {
            tracing::warn!(error = %e, "could not mark the saved password stale");
        }
    }

    fn account(&self, origin: &OriginKey) -> Option<SavedAccount> {
        match self
            .db
            .account_get(&origin.scheme, &origin.host, origin.port, &origin.realm)
        {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "could not read account metadata");
                None
            }
        }
    }

    /// Whether the saved password of `origin` was rejected (now or in an earlier run).
    fn stored_is_stale(&self, origin: &OriginKey) -> bool {
        locked(&self.stored_rejected).contains(origin)
            || self.account(origin).is_some_and(|a| a.stale)
    }

    /// The reason shown for a credentials prompt. The transport only knows its own history; the
    /// broker adds what it remembers across transports and restarts: a typed answer rejected
    /// earlier (`Rejected` at attempt 0) and a saved password marked stale (`Expired`).
    fn prompt_reason(&self, req: &CredentialRequest) -> AuthPromptReason {
        let origin = &req.origin;
        match req.reason {
            PromptReason::Rejected => {
                if locked(&self.awaiting).remove(origin) {
                    locked(&self.typed_rejected).insert(origin.clone());
                }
                AuthPromptReason::Rejected
            }
            PromptReason::Initial if locked(&self.typed_rejected).contains(origin) => {
                AuthPromptReason::Rejected
            }
            PromptReason::Initial if self.stored_is_stale(origin) => AuthPromptReason::Expired,
            other => map_reason(other),
        }
    }

    /// Stops trusting a certificate (row and in-memory pin) and drops cached sessions, so the
    /// next connection asks again. Returns whether it was trusted.
    pub fn forget_certificate(&self, key: &opensvn_ipc::TrustedCertKey) -> Result<bool, AppError> {
        let removed = self.db.certs_remove(key)?;
        // A pin may exist in memory only (its database write failed): the transport trusts it
        // all the same, so dropping it must rebuild the transport too.
        let forgotten = self.trust.forget(&key.host, key.port, &key.sha256);
        if removed || forgotten {
            self.invalidate();
        }
        Ok(removed || forgotten)
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

    /// Signs out of a realm: removes the stored secrets, then the account metadata of each
    /// secret that is gone, and drops cached sessions (which still hold the credentials) so the
    /// next connection asks again.
    ///
    /// Secrets are deleted first and every account is tried: an account whose secret could not
    /// be deleted keeps its row (so signing out again can retry; no secret is left without a
    /// row pointing to it) and is marked stale so the secret is not offered any more. The
    /// failures are reported together at the end.
    pub fn sign_out(&self, realm: &str) -> Result<(), AppError> {
        let accounts = self.db.accounts_of_realm(realm)?;
        let mut failures = Vec::new();
        for account in accounts {
            let origin = OriginKey {
                scheme: account.scheme,
                host: account.host,
                port: account.port,
                realm: realm.to_owned(),
            };
            let where_ = format!("{}:{}", origin.host, origin.port);
            let secret_gone = match &account.username {
                Some(user) => self.secrets.delete(&origin, user).map(|_| ()),
                None => Ok(()),
            };
            locked(&self.awaiting).remove(&origin);
            locked(&self.typed_rejected).remove(&origin);
            match secret_gone {
                Ok(()) => {
                    locked(&self.stored_rejected).remove(&origin);
                    locked(&self.offered).remove(&origin);
                    if let Err(e) = self.db.account_delete(
                        &origin.scheme,
                        &origin.host,
                        origin.port,
                        &origin.realm,
                    ) {
                        failures.push(format!("{where_}: {e}"));
                    }
                }
                Err(e) => {
                    tracing::warn!(host = %origin.host, error = %e, "could not delete a saved password");
                    failures.push(format!("{where_}: {e}"));
                    // Not offered again while it lingers in the store.
                    locked(&self.offered).remove(&origin);
                    self.withhold_stored(&origin);
                }
            }
        }
        self.invalidate();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                AppError::internal("Some saved passwords could not be removed.")
                    .with_detail(failures.join("; ")),
            )
        }
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
            reason: Some(self.prompt_reason(req)),
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
        // Nothing is saved yet: the server has not seen the password. `credentials_accepted`
        // saves it once a response accepted it; a rejected one is never stored.
        locked(&self.awaiting).insert(req.origin.clone());
        Some(Credentials {
            username,
            password,
            save: save && method == AuthMethod::Basic,
            method,
        })
    }

    async fn credentials_accepted(&self, origin: &OriginKey, creds: &Credentials) {
        locked(&self.awaiting).remove(origin);
        locked(&self.typed_rejected).remove(origin);
        if !creds.save || creds.method != AuthMethod::Basic || creds.username.is_empty() {
            return;
        }
        let previous = self.account(origin).and_then(|a| a.username);
        // Secret first: the row (which clears the stale flag) is written only once the store
        // holds the accepted password, so a failed save cannot re-arm an old, rejected one.
        if !self
            .save_secret(origin, &creds.username, &creds.password)
            .await
        {
            return;
        }
        let recorded = self.db.account_upsert(&AccountRecord {
            scheme: &origin.scheme,
            host: &origin.host,
            port: origin.port,
            realm: &origin.realm,
            username: &creds.username,
            negotiate: false,
        });
        if let Err(e) = recorded {
            tracing::warn!(error = %e, "could not record account metadata");
            return;
        }
        // The user replaced the saved password: it may be offered again.
        locked(&self.stored_rejected).remove(origin);
        if let Some(old) = previous.filter(|u| !u.is_empty() && *u != creds.username) {
            // The account now belongs to another user name; its old secret has no row any more.
            let (store, o) = (Arc::clone(&self.secrets), origin.clone());
            match tokio::task::spawn_blocking(move || store.delete(&o, &old)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "could not delete the replaced password"),
                Err(e) => tracing::warn!(error = %e, "credential store worker failed"),
            }
        }
    }

    async fn stored_credentials(&self, req: &CredentialRequest) -> Option<Credentials> {
        if !req.offered.is_empty() && !req.offered.contains(&AuthMethod::Basic) {
            return None;
        }
        let origin = &req.origin;
        if locked(&self.stored_rejected).contains(origin) {
            return None;
        }
        let account = self.account(origin)?;
        if account.stale {
            // Rejected earlier (maybe in another run): the user is asked instead, and background
            // work fails without sending it.
            return None;
        }
        let username = account
            .username
            .filter(|u| !u.is_empty() && !account.negotiate)?;
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
        locked(&self.offered).insert(origin.clone(), username.clone());
        Some(Credentials {
            username,
            password,
            save: true,
            method: AuthMethod::Basic,
        })
    }

    async fn stored_credentials_rejected(&self, req: &CredentialRequest) {
        // In memory first, then one small `UPDATE` on the local database: prompt enough for a
        // call the transport does not race against cancellation.
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
