//! Authentication flow: per-origin credential state, single-flight prompts and Basic headers.
//!
//! Credentials belong to an [`OriginKey`] `(scheme, host, port, realm)`. They are attached only
//! to requests whose scheme, host and port match exactly, so a second server announcing the same
//! realm has to prompt on its own. The first 401 for an origin starts a *flight*: one prompt is
//! shown while every other request that got a 401 for the same origin waits and then reuses the
//! outcome. After three rejected credentials the flight stops with `AuthRequired` to avoid
//! locking a directory account. A prompt that ends without credentials (cancelled or timed out)
//! fails only the requests that were already under way when it ended; it is not a rejected
//! attempt, and the next interactive request prompts again. Typed credentials count as accepted
//! only on a response to a request whose method and URL challenged with a 401 for them: the
//! resent request itself, or (if that one got a server error, a network error or was cancelled)
//! a later request with the same method and URL that sent them up front.
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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// How many challenged `(method, URL)` pairs a flight remembers to confirm typed credentials on.
const MAX_PROTECTED: usize = 256;

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
    /// Value of `AuthManager::denials` when the request was prepared: a prompt that ended
    /// without credentials after this point was one the request waited for.
    seq: u64,
    /// The Basic credentials answer a 401 for this request's URL, so the URL is protected and a
    /// success proves them (a preemptive header on a URL that never challenged proves nothing).
    challenged: bool,
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

/// A prompt answer the server has not accepted yet; reported through
/// `AuthPrompter::credentials_accepted` on the first success.
struct Unconfirmed {
    save: bool,
}

#[derive(Default)]
struct FlightState {
    /// Bumped every time a prompt finishes (with credentials, cancel or give-up).
    generation: u64,
    credentials: Option<Credential>,
    /// Credentials rejected since the last success.
    failures: u8,
    /// The last prompt round ended without credentials (cancelled, timed out or given up).
    denied: bool,
    /// `AuthManager::denials` value stamped when `denied` was set. Only requests prepared
    /// before it fail with the denial; a later request asks again.
    denied_seq: u64,
    /// Stored credentials were already offered for this origin (they are offered once).
    stored_used: bool,
    /// The cached `credentials` came from `AuthPrompter::stored_credentials` and their
    /// rejection has not been reported yet.
    from_stored: bool,
    /// The cached `credentials` were typed at the prompt and no response has accepted them yet.
    unconfirmed: Option<Unconfirmed>,
    /// `(method, URL)` pairs that answered 401 for this origin. A success there proves the
    /// credentials, also when they were sent up front, so a typed password whose resent request
    /// failed without a response (or with a 5xx) is confirmed by the next such request.
    protected: HashSet<(String, String)>,
}

/// Key for `FlightState::protected`: the method and the URL without fragment.
fn protected_key(method: &reqwest::Method, url: &Url) -> (String, String) {
    let mut url = url.clone();
    url.set_fragment(None);
    (method.as_str().to_owned(), url.into())
}

#[derive(Default)]
struct Flight {
    /// Held across a prompt so concurrent 401s wait for one answer.
    gate: tokio::sync::Mutex<()>,
    state: Mutex<FlightState>,
    /// True while a credentials prompt of this flight waits for the user. Unattended requests
    /// fail instead of queueing behind it (a prompt may wait for minutes).
    prompting: tokio::sync::watch::Sender<bool>,
}

/// Clears `Flight::prompting` when the prompt ends, also when the request future is dropped.
struct Prompting<'a>(&'a tokio::sync::watch::Sender<bool>);

impl<'a> Prompting<'a> {
    fn start(flag: &'a tokio::sync::watch::Sender<bool>) -> Self {
        flag.send_replace(true);
        Self(flag)
    }
}

impl Drop for Prompting<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
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
    /// Bumped whenever a prompt round of any flight ends without credentials. Requests record it
    /// in `Attached::seq`, so a denial only fails the requests that were already under way.
    denials: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Drop for AuthManager {
    fn drop(&mut self) {
        for (origin, flight) in lock(&self.flights).iter() {
            if lock(&flight.state).unconfirmed.is_some() {
                // Visible in the log rather than silently lost (#113).
                tracing::info!(
                    host = %origin.host,
                    port = origin.port,
                    "a typed password was never confirmed by the server; it was not saved"
                );
            }
        }
    }
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
            denials: AtomicU64::new(0),
        }
    }

    /// Current denial sequence, recorded in every `Attached`.
    fn seq(&self) -> u64 {
        self.denials.load(Ordering::SeqCst)
    }

    /// Ends a prompt round without credentials: only requests prepared before now (the ones
    /// waiting for this round) fail with it; the next interactive request prompts again.
    fn deny(&self, state: &mut FlightState) {
        state.denied = true;
        state.denied_seq = self.denials.fetch_add(1, Ordering::SeqCst) + 1;
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
            seq: self.seq(),
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
                Ok(Attached {
                    seq: sent.seq,
                    ..Attached::default()
                })
            }
        }
    }

    fn flight(&self, origin: &OriginKey) -> Arc<Flight> {
        Arc::clone(lock(&self.flights).entry(origin.clone()).or_default())
    }

    /// Header to send up front: Negotiate for hosts where it already worked, else Basic (only
    /// ever attached to `https` requests).
    pub(crate) async fn prepare(
        &self,
        method: &reqwest::Method,
        url: &Url,
        channel_binding: Option<Arc<[u8]>>,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let key = HostKey::from_url(url)?;
        // Read before anything else: a denial racing with this call must count as later.
        let seq = self.seq();
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
            return Ok(Attached {
                seq,
                ..Attached::default()
            });
        };
        let flight = self.flight(&origin);
        let state = lock(&flight.state);
        let header = match (&state.credentials, key.scheme.as_str()) {
            (Some(c), "https") => Some(basic_header(&c.username, &c.password)?),
            _ => None,
        };
        // Up front on a method and URL known to challenge: a success proves the credentials.
        let challenged = header.is_some() && state.protected.contains(&protected_key(method, url));
        Ok(Attached {
            header,
            origin: Some(origin),
            generation: state.generation,
            seq,
            challenged,
            ..Attached::default()
        })
    }

    /// Records that the server accepted the credentials carried by `sent` (any response but a
    /// 401). Only a request whose method and URL challenged counts (the resent request, or a
    /// later one that sent them up front): a preemptive header on a URL that never challenged
    /// may have reached a path that does not check it (anonymous read), which proves nothing. The first such response with credentials typed at the
    /// prompt is reported through `AuthPrompter::credentials_accepted`, unless it is a server
    /// error, which proves nothing either.
    ///
    /// For Negotiate, the server's final token (if any) must pass mutual authentication;
    /// otherwise the response is not trusted and `NegotiateFailed` is returned.
    pub(crate) async fn on_success(
        &self,
        url: &Url,
        sent: &Attached,
        status: reqwest::StatusCode,
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
        if let (Some(origin), Some(_), true) = (&sent.origin, &sent.header, sent.challenged) {
            let flight = self.flight(origin);
            let accepted = {
                let mut state = lock(&flight.state);
                if state.generation != sent.generation {
                    None
                } else {
                    state.failures = 0;
                    if status.is_server_error() {
                        if state.unconfirmed.is_some() {
                            tracing::debug!(
                                status = status.as_u16(),
                                "server error; the typed password is confirmed by a later success"
                            );
                        }
                        None
                    } else {
                        match (state.unconfirmed.take(), &state.credentials) {
                            (Some(u), Some(c)) => Some(svn_ra_api::Credentials {
                                username: c.username.clone(),
                                password: c.password.clone(),
                                save: u.save,
                                method: AuthMethod::Basic,
                            }),
                            _ => None,
                        }
                    }
                }
            };
            if let Some(creds) = accepted {
                // Not raced against `cancel`: a typed password the user asked to save is
                // persisted only here, once the server took it.
                self.prompter.credentials_accepted(origin, &creds).await;
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
    ///
    /// A prompt that ends without credentials (cancelled, timed out, or the three-attempt cap)
    /// fails the requests that waited for it, not later ones: the next interactive request
    /// prompts again.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn on_unauthorized(
        &self,
        method: &reqwest::Method,
        url: &Url,
        challenges: &Challenges,
        sent: &Attached,
        channel_binding: Option<Arc<[u8]>>,
        interactive: bool,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let mut next = self
            .answer_unauthorized(url, challenges, sent, channel_binding, interactive, cancel)
            .await?;
        if next.negotiate.is_none() {
            // Basic credentials resent in answer to this URL's challenge. (`seq` was read under
            // the flight gate in `answer_unauthorized`.)
            next.challenged = next.header.is_some();
            if let (true, Some(origin)) = (next.challenged, &next.origin) {
                let flight = self.flight(origin);
                let mut state = lock(&flight.state);
                if state.protected.len() < MAX_PROTECTED {
                    state.protected.insert(protected_key(method, url));
                }
            }
        }
        Ok(next)
    }

    async fn answer_unauthorized(
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
        let _gate = if interactive {
            or_cancel(cancel, flight.gate.lock()).await?
        } else {
            // Background work must not sit behind a prompt that waits for the user.
            let mut prompting = flight.prompting.subscribe();
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return Err(cancel.check().err().unwrap_or_else(|| {
                        SvnError::new(ErrorCode::Cancelled, "operation cancelled")
                    }));
                }
                g = flight.gate.lock() => g,
                _ = prompting.wait_for(|p| *p) => {
                    return Err(auth_refused(
                        "a sign-in prompt for this server is waiting for the user; the \
                         unattended request does not wait for it",
                    ));
                }
            }
        };

        let mut stored_rejected = false;
        let attempt = {
            let mut state = lock(&flight.state);
            // Another request already settled this round: either it has credentials to reuse,
            // or its prompt ended without any after this request was sent.
            let denied_since_sent = state.denied && state.denied_seq > sent.seq;
            let resolved_elsewhere = match sent_generation {
                Some(g) => state.generation != g,
                None => state.credentials.is_some() || denied_since_sent,
            };
            if resolved_elsewhere {
                if let Some(c) = &state.credentials {
                    let header = basic_header(&c.username, &c.password)?;
                    lock(&self.hosts).insert(key, origin.clone());
                    return Ok(Attached {
                        header: Some(header),
                        origin: Some(origin),
                        generation: state.generation,
                        seq: self.seq(),
                        ..Attached::default()
                    });
                }
                if denied_since_sent {
                    return Err(auth_refused("authentication was refused or cancelled"));
                }
            }
            if sent_generation.is_some() {
                // The credentials we sent were rejected.
                state.failures += 1;
                state.credentials = None;
                state.unconfirmed = None;
                stored_rejected = std::mem::take(&mut state.from_stored);
            }
            if state.failures >= MAX_ATTEMPTS {
                state.failures = 0;
                self.deny(&mut state);
                state.generation += 1;
                return Err(auth_refused(&format!(
                    "authentication failed {MAX_ATTEMPTS} times; giving up to avoid locking the account"
                )));
            }
            state.denied = false;
            state.failures
        };

        let offered = challenges.offered_methods();
        // A cancelled prompt does not count as a rejected attempt (`failures` is unchanged), and
        // it only fails the requests that waited for it (see `deny`).
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
            if state.unconfirmed.take().is_some() {
                tracing::info!(
                    host = %origin.host,
                    "a typed password was replaced by the saved one before the server confirmed it"
                );
            }
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
                seq: self.seq(),
                ..Attached::default()
            });
        }
        if !interactive {
            return Err(auth_refused(
                "server requested authentication for an unattended request and no usable \
                 credentials are stored; not prompting",
            ));
        }
        let answer = {
            let _prompting = Prompting::start(&flight.prompting);
            or_cancel(cancel, self.prompter.credentials(&request)).await?
        };
        let mut state = lock(&flight.state);
        state.generation += 1;
        let Some(creds) = answer else {
            self.deny(&mut state);
            return Err(auth_refused("authentication was cancelled"));
        };
        if creds.method != AuthMethod::Basic {
            // Negotiate already failed for this host; there is nothing else to retry with.
            self.deny(&mut state);
            return Err(negotiate_failure(self.negotiate.is_some()));
        }
        let header = basic_header(&creds.username, &creds.password)?;
        state.from_stored = false;
        state.unconfirmed = Some(Unconfirmed { save: creds.save });
        state.credentials = Some(Credential {
            username: creds.username,
            password: creds.password,
        });
        lock(&self.hosts).insert(key, origin.clone());
        Ok(Attached {
            header: Some(header),
            origin: Some(origin),
            generation: state.generation,
            // Read while the flight gate is held: a round ending concurrently cannot make it
            // stale (#113).
            seq: self.seq(),
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
