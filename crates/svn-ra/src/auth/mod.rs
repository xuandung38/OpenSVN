//! Authentication flow: per-origin credential state, single-flight prompts and Basic headers.
//!
//! Credentials belong to an [`OriginKey`] `(scheme, host, port, realm)`. They are attached only
//! to requests whose scheme, host and port match exactly, so a second server announcing the same
//! realm has to prompt on its own. The first 401 for an origin starts a *flight*: one prompt is
//! shown while every other request that got a 401 for the same origin waits and then reuses the
//! outcome. After three rejected credentials the flight stops with `AuthRequired` to avoid
//! locking a directory account.
//!
//! Negotiate (SPNEGO) is not implemented yet: a server that offers it is surfaced to the prompter
//! through the `offered` list, and the transport never falls back to Basic on its own.

mod basic;
mod challenge;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use reqwest::header::HeaderValue;
use secrecy::SecretString;
use svn_ra_api::{AuthMethod, AuthPrompter, CredentialRequest, PromptReason};
use svn_types::{CancelToken, ErrorCode, OriginKey, Result, SvnError};
use url::Url;

pub use basic::basic_header;
pub use challenge::Challenges;

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
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AuthManager {
    pub(crate) fn new(prompter: Arc<dyn AuthPrompter>) -> Self {
        Self {
            prompter,
            flights: Mutex::default(),
            hosts: Mutex::default(),
        }
    }

    fn flight(&self, origin: &OriginKey) -> Arc<Flight> {
        Arc::clone(lock(&self.flights).entry(origin.clone()).or_default())
    }

    /// Header to send up front. Basic is only ever attached to `https` requests.
    pub(crate) fn prepare(&self, url: &Url) -> Result<Attached> {
        let key = HostKey::from_url(url)?;
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
        })
    }

    /// Records that the server accepted the credentials carried by `sent`.
    pub(crate) fn on_success(&self, sent: &Attached) {
        if let (Some(origin), Some(_)) = (&sent.origin, &sent.header) {
            let flight = self.flight(origin);
            let mut state = lock(&flight.state);
            if state.generation == sent.generation {
                state.failures = 0;
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
        let Some(realm) = challenges.basic_realm.as_deref() else {
            return Err(SvnError::new(
                ErrorCode::AuthRequired,
                format!(
                    "server offers no supported authentication scheme (offered: {}); \
                     Negotiate is not available yet",
                    challenges.describe()
                ),
            ));
        };
        if key.scheme != "https" {
            return Err(SvnError::new(
                ErrorCode::AuthRequired,
                "refusing to send Basic credentials over plain http; use an https URL",
            ));
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
        let reason = if attempt == 0 {
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
            state.denied = true;
            return Err(auth_refused(
                "Negotiate authentication is not available yet; choose Basic",
            ));
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

fn auth_refused(msg: &str) -> SvnError {
    SvnError::new(ErrorCode::AuthRequired, msg)
}
