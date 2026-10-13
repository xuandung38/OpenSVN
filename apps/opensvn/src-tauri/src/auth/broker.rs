//! See the module docs in `mod.rs`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use opensvn_ipc::{
    AppError, AuthPrompt, AuthPromptKind, AuthPromptReason, AuthResponse,
    CertDecision as IpcDecision, CertSummary, ErrorCode,
};
use secrecy::SecretString;
use svn_ra_api::{
    AuthMethod, AuthPrompter, CertDecision, CertFailure, CertInfo, CredentialRequest, Credentials,
    PromptReason,
};
use svn_types::OriginKey;
use tokio::sync::oneshot;

use crate::db::{AccountRecord, AppDb};
use crate::ops::AppEvents;

/// How long a prompt waits for the user before it counts as cancelled.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Removes stored secrets of a signed-out origin (implemented over the OS credential store).
pub trait CredentialPurge: Send + Sync {
    /// Deletes the secrets saved for `scheme://host:port` and `realm`.
    fn purge(&self, scheme: &str, host: &str, port: u16, realm: &str) -> Result<(), AppError>;
}

/// Purger used while no credential store is wired: there is nothing to remove.
pub struct NoSecretStore;

impl CredentialPurge for NoSecretStore {
    fn purge(&self, _: &str, _: &str, _: u16, _: &str) -> Result<(), AppError> {
        Ok(())
    }
}

struct Pending {
    reply: oneshot::Sender<AuthResponse>,
    wants_certificate: bool,
}

/// Bridges the transport's [`AuthPrompter`] to the UI.
pub struct AuthBroker {
    events: Arc<dyn AppEvents>,
    db: AppDb,
    purge: Arc<dyn CredentialPurge>,
    timeout: Duration,
    pending: Mutex<HashMap<String, Pending>>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn failure_text(f: &CertFailure) -> String {
    match f {
        CertFailure::UnknownIssuer => "unknown-issuer".into(),
        CertFailure::SelfSigned => "self-signed".into(),
        CertFailure::Expired => "expired".into(),
        CertFailure::NotYetValid => "not-yet-valid".into(),
        CertFailure::HostnameMismatch => "hostname-mismatch".into(),
        CertFailure::Revoked => "revoked".into(),
        CertFailure::Changed { .. } => "changed".into(),
        CertFailure::Other(m) => format!("other: {m}"),
        // `CertFailure` is non-exhaustive: a new reason is shown as unknown rather than guessed.
        _ => "unknown".into(),
    }
}

fn map_reason(r: PromptReason) -> AuthPromptReason {
    match r {
        PromptReason::Rejected => AuthPromptReason::Rejected,
        PromptReason::NegotiateFailed => AuthPromptReason::NegotiateFailed,
        PromptReason::Expired => AuthPromptReason::Expired,
        _ => AuthPromptReason::Initial,
    }
}

impl AuthBroker {
    /// Broker emitting prompts through `events`, persisting metadata in `db`.
    pub fn new(events: Arc<dyn AppEvents>, db: AppDb, purge: Arc<dyn CredentialPurge>) -> Self {
        Self::with_timeout(events, db, purge, PROMPT_TIMEOUT)
    }

    /// Like [`AuthBroker::new`] with a custom prompt timeout.
    pub fn with_timeout(
        events: Arc<dyn AppEvents>,
        db: AppDb,
        purge: Arc<dyn CredentialPurge>,
        timeout: Duration,
    ) -> Self {
        Self {
            events,
            db,
            purge,
            timeout,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
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
            AuthResponse::Credentials { .. } => !entry.wants_certificate,
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

    /// Signs out of a realm: removes account metadata and stored secrets.
    pub fn sign_out(&self, realm: &str) -> Result<(), AppError> {
        for (scheme, host, port) in self.db.account_delete_realm(realm)? {
            self.purge.purge(&scheme, &host, port, realm)?;
        }
        Ok(())
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
        };
        let AuthResponse::Credentials {
            username,
            password,
            save,
        } = self.ask(prompt, false).await?
        else {
            return None;
        };
        let method = if req.offered.contains(&AuthMethod::Basic) || req.offered.is_empty() {
            AuthMethod::Basic
        } else {
            req.offered[0]
        };
        if save
            && let Err(e) = self.db.account_upsert(&AccountRecord {
                scheme: &req.origin.scheme,
                host: &req.origin.host,
                port: req.origin.port,
                realm: &req.origin.realm,
                username: &username,
                negotiate: method == AuthMethod::Negotiate,
            })
        {
            tracing::warn!(error = %e, "could not record account metadata");
        }
        Some(Credentials {
            username,
            password: SecretString::from(password.expose().to_owned()),
            save,
            method,
        })
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
        let old_sha256 = match &info.failure {
            CertFailure::Changed { old_sha256, .. } => Some(hex(old_sha256)),
            _ => None,
        };
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
                failure: failure_text(&info.failure),
                old_sha256,
            }),
            reason: None,
            attempt: 0,
        };
        let Some(AuthResponse::Certificate { decision }) = self.ask(prompt, true).await else {
            return CertDecision::Reject;
        };
        match decision {
            IpcDecision::Reject => CertDecision::Reject,
            IpcDecision::AcceptOnce => CertDecision::AcceptOnce,
            IpcDecision::AcceptAlways if !permanent_ok => CertDecision::AcceptOnce,
            IpcDecision::AcceptAlways => {
                let expires = opensvn_ipc::time_fmt::timestamp_to_iso(info.not_after);
                if let Err(e) =
                    self.db
                        .cert_trust(&info.host, info.port, &fingerprint, &info.subject, &expires)
                {
                    tracing::warn!(error = %e, "could not store trusted certificate");
                    return CertDecision::AcceptOnce;
                }
                CertDecision::AcceptAlways
            }
        }
    }
}
