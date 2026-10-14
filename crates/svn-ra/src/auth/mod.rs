//! Authentication flow: per-origin credential state, single-flight prompts and Basic headers.
//!
//! Credentials belong to an [`OriginKey`] `(scheme, host, port, realm)`. They are attached only
//! to requests whose scheme, host and port match exactly, so a second server announcing the same
//! realm has to prompt on its own. The first 401 for an origin starts a *flight*: one prompt is
//! shown while every other request that got a 401 for the same origin waits and then reuses the
//! outcome. After three rejected credentials the flight stops with `AuthRequired` to avoid
//! locking a directory account.
//!
//! Negotiate (SPNEGO/Kerberos, see [`negotiate`]) is tried first when the server offers it, the
//! build has a provider and the URL is `https`. On plain `http` no Negotiate token is ever sent
//! and the request fails with `ErrorCode::NegotiateFailed` ("use an https:// URL"). When it fails
//! for a host (no ticket, rejected ticket, IP host, unavailable), the host is remembered for
//! [`NEGOTIATE_RETRY_AFTER`] and the user is prompted with [`PromptReason::NegotiateFailed`] if
//! Basic is also offered; otherwise the request fails with `ErrorCode::NegotiateFailed`. The
//! transport never switches to Basic without asking. After a successful exchange, later requests
//! to the host start Negotiate up front with a fresh context (each token is single use), which
//! also lets one-shot bodies through. If such an up-front token is rejected, one fresh context
//! answers the server's challenge before the host counts as failed. NTLM is not supported.

mod basic;
mod challenge;
pub mod negotiate;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::HeaderValue;
use secrecy::SecretString;
use svn_ra_api::{AuthMethod, AuthPrompter, CredentialRequest, PromptReason};
use svn_types::{CancelToken, ErrorCode, OriginKey, Result, SvnError};
use url::Url;

pub use basic::basic_header;
pub use challenge::Challenges;
pub use negotiate::{NegotiateContext, NegotiateProvider, NegotiateStep};

use negotiate::PendingNegotiate;

/// Maximum number of rejected credentials per origin before giving up.
pub const MAX_ATTEMPTS: u8 = 3;

/// How long a Negotiate failure is remembered for a host. Afterwards SSO is tried again, so a
/// ticket obtained later (`kinit`, renewed logon session) is picked up without a restart.
pub const NEGOTIATE_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// `(scheme, host, port)` of a request; the realm is only known after a challenge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct HostKey {
    scheme: String,
    host: String,
    port: u16,
}

impl HostKey {
    /// Derives the key from a request URL.
    pub(crate) fn from_url(url: &Url) -> Result<Self> {
        let host = url
            .host_str()
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "request URL has no host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| SvnError::new(ErrorCode::Internal, "request URL has no port"))?;
        Ok(Self {
            scheme: url.scheme().to_ascii_lowercase(),
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    fn origin(&self, realm: &str) -> OriginKey {
        OriginKey {
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            port: self.port,
            realm: realm.to_owned(),
        }
    }
}

/// What a request carried, so a later 401 can be attributed correctly.
#[derive(Debug, Clone, Default)]
pub(crate) struct Attached {
    /// `Authorization` value to send, if credentials are known.
    pub(crate) header: Option<HeaderValue>,
    /// Origin the credentials belong to.
    origin: Option<OriginKey>,
    /// Flight generation the header came from.
    generation: u64,
    /// Negotiate context whose token is in `header`, waiting for the server's final token.
    negotiate: Option<Arc<PendingNegotiate>>,
    /// The Negotiate token was sent up front (not in answer to a challenge).
    proactive: bool,
}

impl Attached {
    /// The request carried a Negotiate token.
    pub(crate) fn is_negotiate(&self) -> bool {
        self.negotiate.is_some()
    }
}

struct Credential {
    username: String,
    password: SecretString,
}

#[derive(Default)]
struct FlightState {
    /// Bumped every time a prompt finishes (with credentials, cancel or give-up).
    generation: u64,
    credentials: Option<Credential>,
    /// Credentials rejected since the last success.
    failures: u8,
    /// The last prompt round ended without credentials.
    denied: bool,
    /// Stored credentials were already offered for this origin (they are offered once).
    stored_used: bool,
    /// The cached `credentials` came from `AuthPrompter::stored_credentials` and their
    /// rejection has not been reported yet.
    from_stored: bool,
}

#[derive(Default)]
struct Flight {
    /// Held across a prompt so concurrent 401s wait for one answer.
    gate: tokio::sync::Mutex<()>,
    state: Mutex<FlightState>,
}

/// Per-transport authentication state.
pub(crate) struct AuthManager {
    prompter: Arc<dyn AuthPrompter>,
    flights: Mutex<HashMap<OriginKey, Arc<Flight>>>,
    /// Last origin (realm) that produced credentials for a host, for preemptive Basic.
    hosts: Mutex<HashMap<HostKey, OriginKey>>,
    /// Platform Negotiate provider; `None` when the build has no SSO support.
    negotiate: Option<Arc<dyn NegotiateProvider>>,
    /// Hosts where Negotiate failed recently, with when and why; they go straight to the Basic
    /// prompt until the entry is older than `negotiate_retry_after`.
    negotiate_failed: Mutex<HashMap<HostKey, (Instant, String)>>,
    negotiate_retry_after: Duration,
    /// Hosts where Negotiate succeeded; later requests start it up front.
    negotiate_ok: Mutex<HashSet<HostKey>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sets `FlightState::from_stored` again unless disarmed, so a stored-credential rejection
/// whose report was interrupted (the request future dropped mid-hook) is not forgotten.
struct RestoreFromStored<'a>(Option<&'a Mutex<FlightState>>);

impl RestoreFromStored<'_> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for RestoreFromStored<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.0 {
            lock(state).from_stored = true;
        }
    }
}

impl AuthManager {
    pub(crate) fn new(
        prompter: Arc<dyn AuthPrompter>,
        negotiate: Option<Arc<dyn NegotiateProvider>>,
    ) -> Self {
        Self {
            prompter,
            flights: Mutex::default(),
            hosts: Mutex::default(),
            negotiate,
            negotiate_failed: Mutex::default(),
            negotiate_retry_after: NEGOTIATE_RETRY_AFTER,
            negotiate_ok: Mutex::default(),
        }
    }

    /// Replaces the Negotiate provider (tests inject a fake one).
    pub(crate) fn set_negotiate(&mut self, provider: Option<Arc<dyn NegotiateProvider>>) {
        self.negotiate = provider;
    }

    /// Changes how long a Negotiate failure is remembered per host.
    pub(crate) fn set_negotiate_retry_after(&mut self, after: Duration) {
        self.negotiate_retry_after = after;
    }

    fn mark_negotiate_failed(&self, key: &HostKey, reason: &str) {
        lock(&self.negotiate_ok).remove(key);
        lock(&self.negotiate_failed).insert(key.clone(), (Instant::now(), reason.to_owned()));
    }

    /// The reason Negotiate recently failed for `key`, dropping an expired entry.
    fn recent_negotiate_failure(&self, key: &HostKey) -> Option<String> {
        let mut failed = lock(&self.negotiate_failed);
        match failed.get(key) {
            Some((at, _)) if at.elapsed() >= self.negotiate_retry_after => {
                failed.remove(key);
                None
            }
            Some((_, reason)) => Some(reason.clone()),
            None => None,
        }
    }

    /// Starts a fresh Negotiate context for `key` (never reuses a token).
    async fn start_negotiate(
        &self,
        provider: &Arc<dyn NegotiateProvider>,
        key: &HostKey,
        channel_binding: Option<Arc<[u8]>>,
        proactive: bool,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let (header, pending) =
            negotiate::start(provider, &key.host, channel_binding, cancel).await?;
        Ok(Attached {
            header: Some(header),
            negotiate: Some(pending),
            proactive,
            ..Attached::default()
        })
    }

    /// Credentials for resending `sent` after a transient failure. A Negotiate token is single
    /// use (servers keep a replay cache), so a fresh context is started; if that fails, the
    /// request goes out without credentials and the server's challenge decides what follows.
    pub(crate) async fn renew(
        &self,
        url: &Url,
        sent: &Attached,
        channel_binding: Option<Arc<[u8]>>,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let Some(provider) = sent.negotiate.as_ref().and(self.negotiate.as_ref()) else {
            return Ok(sent.clone());
        };
        let key = HostKey::from_url(url)?;
        match self
            .start_negotiate(provider, &key, channel_binding, sent.proactive, cancel)
            .await
        {
            Ok(next) => Ok(next),
            Err(e) if e.code == ErrorCode::Cancelled => Err(e),
            Err(e) => {
                tracing::debug!(error = %e.message, "cannot renew the Negotiate token");
                Ok(Attached::default())
            }
        }
    }

    /// User name of the Basic credentials cached for the host of `url`, if any.
    pub(crate) fn basic_user(&self, url: &Url) -> Option<String> {
        let key = HostKey::from_url(url).ok()?;
        let origin = lock(&self.hosts).get(&key).cloned()?;
        let flight = self.flight(&origin);
        let state = lock(&flight.state);
        state.credentials.as_ref().map(|c| c.username.clone())
    }

    fn flight(&self, origin: &OriginKey) -> Arc<Flight> {
        Arc::clone(lock(&self.flights).entry(origin.clone()).or_default())
    }

    /// Header to send up front: Negotiate for hosts where it already worked, else Basic (only
    /// ever attached to `https` requests).
    pub(crate) async fn prepare(
        &self,
        url: &Url,
        channel_binding: Option<Arc<[u8]>>,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let key = HostKey::from_url(url)?;
        if let Some(provider) = &self.negotiate
            && key.scheme == "https"
            && lock(&self.negotiate_ok).contains(&key)
        {
            match self
                .start_negotiate(provider, &key, channel_binding, true, cancel)
                .await
            {
                Ok(next) => return Ok(next),
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(e) => {
                    // Let the server's challenge decide what happens next.
                    tracing::debug!(error = %e.message, "preemptive Negotiate failed");
                    lock(&self.negotiate_ok).remove(&key);
                }
            }
        }
        let Some(origin) = lock(&self.hosts).get(&key).cloned() else {
            return Ok(Attached::default());
        };
        let flight = self.flight(&origin);
        let state = lock(&flight.state);
        let header = match (&state.credentials, key.scheme.as_str()) {
            (Some(c), "https") => Some(basic_header(&c.username, &c.password)?),
            _ => None,
        };
        Ok(Attached {
            header,
            origin: Some(origin),
            generation: state.generation,
            ..Attached::default()
        })
    }

    /// Records that the server accepted the credentials carried by `sent`.
    ///
    /// For Negotiate, the server's final token (if any) must pass mutual authentication;
    /// otherwise the response is not trusted and `NegotiateFailed` is returned.
    pub(crate) async fn on_success(
        &self,
        url: &Url,
        sent: &Attached,
        headers: &reqwest::header::HeaderMap,
        cancel: &CancelToken,
    ) -> Result<()> {
        if let Some(pending) = &sent.negotiate {
            let key = HostKey::from_url(url)?;
            match negotiate::finish(pending, headers, cancel).await {
                Ok(verified) => {
                    // Without a final token mutual authentication is unavailable; the token we
                    // sent was still checked to be Kerberos, and only over https.
                    if !verified {
                        tracing::debug!("server sent no Negotiate mutual-authentication token");
                    }
                    lock(&self.negotiate_failed).remove(&key);
                    lock(&self.negotiate_ok).insert(key);
                }
                Err(e) => {
                    if e.code != ErrorCode::Cancelled {
                        self.mark_negotiate_failed(&key, &e.message);
                    }
                    return Err(e);
                }
            }
            return Ok(());
        }
        if let (Some(origin), Some(_)) = (&sent.origin, &sent.header) {
            let flight = self.flight(origin);
            let mut state = lock(&flight.state);
            if state.generation == sent.generation {
                state.failures = 0;
            }
        }
        Ok(())
    }

    /// Tries Negotiate for a 401 that offers it. `Ok(Some)` is the request to retry with;
    /// `Ok(None)` means Negotiate is unusable for this host and the caller falls back.
    async fn try_negotiate(
        &self,
        key: &HostKey,
        challenges: &Challenges,
        sent: &Attached,
        channel_binding: Option<Arc<[u8]>>,
        cancel: &CancelToken,
    ) -> Result<Option<Attached>> {
        if !challenges.negotiate {
            return Ok(None);
        }
        if key.scheme != "https" {
            // A Kerberos authenticator in clear text can be captured and relayed within the
            // clock-skew window, and nothing authenticates the response. Never send one.
            return Err(negotiate::negotiate_failed(
                "single sign-on (Negotiate/Kerberos) is only used over https; use an https:// URL \
                 for this repository",
            ));
        }
        if sent.negotiate.is_some() {
            lock(&self.negotiate_ok).remove(key);
            if !sent.proactive {
                // Our token was rejected, or the server asked for another leg (NTLM-style),
                // which cannot be done without pinning the connection.
                tracing::debug!("server rejected the Negotiate token");
                self.mark_negotiate_failed(key, "the server rejected the Kerberos ticket");
                return Ok(None);
            }
            // An up-front token was rejected (clock skew, expired ticket, replay cache): answer
            // the challenge once with a fresh context before giving up on the host.
            tracing::debug!("server rejected the up-front Negotiate token; retrying once");
        } else if self.recent_negotiate_failure(key).is_some() {
            return Ok(None);
        }
        let Some(provider) = &self.negotiate else {
            return Ok(None);
        };
        match self
            .start_negotiate(provider, key, channel_binding, false, cancel)
            .await
        {
            Ok(next) => Ok(Some(next)),
            Err(e) if e.code == ErrorCode::Cancelled => Err(e),
            Err(e) => {
                tracing::debug!(error = %e.message, "cannot start Negotiate");
                self.mark_negotiate_failed(key, &e.message);
                Ok(None)
            }
        }
    }

    /// Handles a 401: returns credentials to retry with, or the reason authentication failed.
    ///
    /// Before the first prompt for an origin the prompter's stored credentials are tried
    /// silently. When `interactive` is false nothing is ever prompted: without cached or
    /// stored credentials (or once they are rejected) the request fails with `AuthRequired`.
    /// Negotiate never prompts, so it is tried in both modes.
    pub(crate) async fn on_unauthorized(
        &self,
        url: &Url,
        challenges: &Challenges,
        sent: &Attached,
        channel_binding: Option<Arc<[u8]>>,
        interactive: bool,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let key = HostKey::from_url(url)?;
        if let Some(next) = self
            .try_negotiate(&key, challenges, sent, channel_binding, cancel)
            .await?
        {
            return Ok(next);
        }
        let Some(realm) = challenges.basic_realm.as_deref() else {
            if challenges.negotiate {
                let err = negotiate_failure(self.negotiate.is_some());
                return Err(match self.recent_negotiate_failure(&key) {
                    Some(reason) => err.with_child(negotiate::negotiate_failed(reason)),
                    None => err,
                });
            }
            return Err(SvnError::new(
                ErrorCode::AuthRequired,
                format!(
                    "server offers no supported authentication scheme (offered: {})",
                    challenges.describe()
                ),
            ));
        };
        if key.scheme != "https" {
            let msg = "refusing to send Basic credentials over plain http; use an https URL";
            if challenges.negotiate {
                return Err(negotiate_failure(self.negotiate.is_some())
                    .with_child(SvnError::new(ErrorCode::AuthRequired, msg)));
            }
            return Err(SvnError::new(ErrorCode::AuthRequired, msg));
        }
        let origin = key.origin(realm);
        let flight = self.flight(&origin);
        let sent_generation = match (&sent.origin, &sent.header) {
            (Some(o), Some(_)) if *o == origin => Some(sent.generation),
            _ => None,
        };
        // Generation observed before queueing: a request that carried no credentials may have
        // been sent at any earlier generation, so only the carried case is compared.
        let _gate = or_cancel(cancel, flight.gate.lock()).await?;

        let mut stored_rejected = false;
        let attempt = {
            let mut state = lock(&flight.state);
            let resolved_elsewhere = match sent_generation {
                Some(g) => state.generation != g,
                None => state.credentials.is_some() || state.generation != sent.generation,
            };
            if resolved_elsewhere {
                if let Some(c) = &state.credentials {
                    let header = basic_header(&c.username, &c.password)?;
                    lock(&self.hosts).insert(key, origin.clone());
                    return Ok(Attached {
                        header: Some(header),
                        origin: Some(origin),
                        generation: state.generation,
                        ..Attached::default()
                    });
                }
                if state.denied {
                    return Err(auth_refused("authentication was refused or cancelled"));
                }
            }
            if sent_generation.is_some() {
                // The credentials we sent were rejected.
                state.failures += 1;
                state.credentials = None;
                stored_rejected = std::mem::take(&mut state.from_stored);
            }
            if state.failures >= MAX_ATTEMPTS {
                state.failures = 0;
                state.denied = true;
                state.generation += 1;
                return Err(auth_refused(&format!(
                    "authentication failed {MAX_ATTEMPTS} times; giving up to avoid locking the account"
                )));
            }
            state.denied = false;
            state.failures
        };

        let offered = challenges.offered_methods();
        // A cancelled prompt leaves the flight untouched so other requests can still ask.
        // Reaching the prompt while Negotiate is offered means it failed or is unavailable.
        let reason = if attempt == 0 && challenges.negotiate {
            PromptReason::NegotiateFailed
        } else if attempt == 0 {
            PromptReason::Initial
        } else {
            PromptReason::Rejected
        };
        let request = CredentialRequest::new(origin.clone(), u32::from(attempt), offered, reason);
        if stored_rejected {
            // Lets the app stop offering a password the server refused (lockout protection).
            // Deliberately not raced against `cancel`: the flag was already cleared, so a
            // cancel landing mid-hook would lose the report for good and leave the stale
            // password offered to the next transport. The hook is synchronous bookkeeping
            // that must return promptly; a pending cancel is honoured right after it.
            // If this future is dropped mid-hook anyway, the guard puts the flag back so the
            // next 401 for these credentials reports it.
            let restore = RestoreFromStored(Some(&flight.state));
            self.prompter.stored_credentials_rejected(&request).await;
            restore.disarm();
            cancel.check()?;
        }
        let try_stored = attempt == 0 && !lock(&flight.state).stored_used;
        if try_stored
            && let Some(creds) = or_cancel(cancel, self.prompter.stored_credentials(&request))
                .await?
                .filter(|c| c.method == AuthMethod::Basic)
        {
            let header = basic_header(&creds.username, &creds.password)?;
            let mut state = lock(&flight.state);
            state.stored_used = true;
            state.from_stored = true;
            state.generation += 1;
            state.credentials = Some(Credential {
                username: creds.username,
                password: creds.password,
            });
            lock(&self.hosts).insert(key, origin.clone());
            return Ok(Attached {
                header: Some(header),
                origin: Some(origin),
                generation: state.generation,
                ..Attached::default()
            });
        }
        if !interactive {
            return Err(auth_refused(
                "server requested authentication for an unattended request and no usable \
                 credentials are stored; not prompting",
            ));
        }
        let answer = or_cancel(cancel, self.prompter.credentials(&request)).await?;
        let mut state = lock(&flight.state);
        state.generation += 1;
        let Some(creds) = answer else {
            state.denied = true;
            return Err(auth_refused("authentication was cancelled"));
        };
        if creds.method != AuthMethod::Basic {
            // Negotiate already failed for this host; there is nothing else to retry with.
            state.denied = true;
            return Err(negotiate_failure(self.negotiate.is_some()));
        }
        let header = basic_header(&creds.username, &creds.password)?;
        state.from_stored = false;
        state.credentials = Some(Credential {
            username: creds.username,
            password: creds.password,
        });
        lock(&self.hosts).insert(key, origin.clone());
        Ok(Attached {
            header: Some(header),
            origin: Some(origin),
            generation: state.generation,
            ..Attached::default()
        })
    }
}

/// Runs `fut` unless `cancel` fires first.
pub(crate) async fn or_cancel<T>(
    cancel: &CancelToken,
    fut: impl std::future::Future<Output = T>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(cancel
            .check()
            .err()
            .unwrap_or_else(|| SvnError::new(ErrorCode::Cancelled, "operation cancelled"))),
        v = fut => Ok(v),
    }
}

/// Error for a server that wants Negotiate when it cannot be used.
fn negotiate_failure(available: bool) -> SvnError {
    negotiate::negotiate_failed(if available {
        "single sign-on (Negotiate/Kerberos) failed: check that you have a Kerberos ticket and \
         that the repository URL uses the server's full host name"
    } else {
        "the server requires single sign-on (Negotiate/Kerberos), which this build does not support"
    })
}

fn auth_refused(msg: &str) -> SvnError {
    SvnError::new(ErrorCode::AuthRequired, msg)
}
