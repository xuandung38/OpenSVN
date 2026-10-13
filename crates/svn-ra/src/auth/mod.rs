//! Authentication flow: per-origin credential state, single-flight prompts and Basic headers.
//!
//! Credentials belong to an [`OriginKey`] `(scheme, host, port, realm)`. They are attached only
//! to requests whose scheme, host and port match exactly, so a second server announcing the same
//! realm has to prompt on its own. The first 401 for an origin starts a *flight*: one prompt is
//! shown while every other request that got a 401 for the same origin waits and then reuses the
//! outcome. After three rejected credentials the flight stops with `AuthRequired` to avoid
//! locking a directory account.
//!
//! Negotiate (SPNEGO/Kerberos, see [`negotiate`]) is tried first when the server offers it and the
//! build has a provider. When it fails for a host (no ticket, rejected ticket, unavailable), the
//! host is remembered and the user is prompted with [`PromptReason::NegotiateFailed`] if Basic is
//! also offered; otherwise the request fails with `ErrorCode::NegotiateFailed`. The transport never
//! switches to Basic without asking. After a successful exchange, later requests to the host start
//! Negotiate up front (each token is single use), which also lets one-shot bodies through.
//! NTLM is not supported.

mod basic;
mod challenge;
pub mod negotiate;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

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
    /// Hosts where Negotiate failed in this session; they go straight to the Basic prompt.
    negotiate_failed: Mutex<HashSet<HostKey>>,
    /// Hosts where Negotiate succeeded; later requests start it up front.
    negotiate_ok: Mutex<HashSet<HostKey>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
            negotiate_ok: Mutex::default(),
        }
    }

    /// Replaces the Negotiate provider (tests inject a fake one).
    pub(crate) fn set_negotiate(&mut self, provider: Option<Arc<dyn NegotiateProvider>>) {
        self.negotiate = provider;
    }

    fn flight(&self, origin: &OriginKey) -> Arc<Flight> {
        Arc::clone(lock(&self.flights).entry(origin.clone()).or_default())
    }

    /// Header to send up front: Negotiate for hosts where it already worked, else Basic (only
    /// ever attached to `https` requests).
    pub(crate) async fn prepare(&self, url: &Url, cancel: &CancelToken) -> Result<Attached> {
        let key = HostKey::from_url(url)?;
        if let Some(provider) = &self.negotiate
            && lock(&self.negotiate_ok).contains(&key)
        {
            match negotiate::start(provider, &key.host, cancel).await {
                Ok((header, pending)) => {
                    return Ok(Attached {
                        header: Some(header),
                        negotiate: Some(pending),
                        ..Attached::default()
                    });
                }
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
            negotiate: None,
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
                    if !verified {
                        tracing::debug!("server sent no Negotiate mutual-authentication token");
                    }
                    lock(&self.negotiate_failed).remove(&key);
                    lock(&self.negotiate_ok).insert(key);
                }
                Err(e) => {
                    if e.code != ErrorCode::Cancelled {
                        lock(&self.negotiate_ok).remove(&key);
                        lock(&self.negotiate_failed).insert(key);
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
        cancel: &CancelToken,
    ) -> Result<Option<Attached>> {
        if !challenges.negotiate {
            return Ok(None);
        }
        if sent.negotiate.is_some() {
            // Our token was rejected, or the server asked for another leg (NTLM-style), which
            // cannot be done without pinning the connection.
            tracing::debug!("server rejected the Negotiate token");
            lock(&self.negotiate_ok).remove(key);
            lock(&self.negotiate_failed).insert(key.clone());
            return Ok(None);
        }
        if lock(&self.negotiate_failed).contains(key) {
            return Ok(None);
        }
        let Some(provider) = &self.negotiate else {
            return Ok(None);
        };
        match negotiate::start(provider, &key.host, cancel).await {
            Ok((header, pending)) => Ok(Some(Attached {
                header: Some(header),
                negotiate: Some(pending),
                ..Attached::default()
            })),
            Err(e) if e.code == ErrorCode::Cancelled => Err(e),
            Err(e) => {
                tracing::debug!(error = %e.message, "cannot start Negotiate");
                lock(&self.negotiate_failed).insert(key.clone());
                Ok(None)
            }
        }
    }

    /// Handles a 401: returns credentials to retry with, or the reason authentication failed.
    pub(crate) async fn on_unauthorized(
        &self,
        url: &Url,
        challenges: &Challenges,
        sent: &Attached,
        cancel: &CancelToken,
    ) -> Result<Attached> {
        let key = HostKey::from_url(url)?;
        if let Some(next) = self.try_negotiate(&key, challenges, sent, cancel).await? {
            return Ok(next);
        }
        let Some(realm) = challenges.basic_realm.as_deref() else {
            if challenges.negotiate {
                return Err(negotiate_failure(self.negotiate.is_some()));
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
                        negotiate: None,
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
        state.credentials = Some(Credential {
            username: creds.username,
            password: creds.password,
        });
        lock(&self.hosts).insert(key, origin.clone());
        Ok(Attached {
            header: Some(header),
            origin: Some(origin),
            generation: state.generation,
            negotiate: None,
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
