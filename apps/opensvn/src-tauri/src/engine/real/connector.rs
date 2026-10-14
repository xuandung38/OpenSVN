//! [`HttpConnector`]: the app's [`SessionConnector`] over the HTTP transport.
//!
//! * **One transport.** Every session shares one long-lived [`HttpTransport`], so credentials,
//!   accepted certificates, Negotiate state and the "saved password already tried" bookkeeping
//!   are per app, not per operation. A transport per operation would offer a stale saved password
//!   again on every operation, and a few background checks would lock a domain account.
//! * **One open per repository.** A session is opened (one `OPTIONS`) at the URL that was asked
//!   for, so path-based authorization applies (a user allowed on `/trunk` only can still connect),
//!   and is cached under its repository root. Later requests for any URL below that root reuse it
//!   through [`HttpRaSession::reparent`], which sends nothing. Lookup is by longest root prefix;
//!   scheme and host compare case-insensitively.
//! * **Background work never prompts.** With `ConnectOptions::interactive == false` the session
//!   is [`HttpRaSession::unattended`]: saved credentials and accepted certificates are used, a
//!   request that would need the user fails with `auth-required` instead.
//! * **Invalidation.** A change of the network settings, a sign-out or a removed certificate
//!   replaces the transport and empties the cache, so the next operation connects (and, if
//!   needed, asks) again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use opensvn_ipc::{ProxySetting, Settings};
use svn_ra::ops::HttpRaSession;
use svn_ra::transport::{HttpTransport, ProxyConfig, TransportConfig, TrustStore};
use svn_ra_api::{AuthPrompter, RaSession};
use svn_types::{CancelToken, ErrorCode, RepoUrl, SvnError};
use tokio::sync::watch;

use super::{Anchor, ConnectOptions, SessionConnector};

/// The settings the transport is built from; a change of any of them rebuilds it.
#[derive(Debug, Clone, PartialEq)]
struct NetSettings {
    max_connections: usize,
    timeout: Duration,
    proxy: ProxySetting,
    negotiate: bool,
}

impl NetSettings {
    fn of(s: &Settings) -> Self {
        Self {
            max_connections: usize::from(s.network_max_connections),
            timeout: Duration::from_secs(u64::from(s.network_timeout_sec.max(1))),
            proxy: s.network_proxy.clone(),
            negotiate: s.network_negotiate,
        }
    }

    fn transport_config(&self) -> TransportConfig {
        let defaults = TransportConfig::default();
        TransportConfig {
            // Clamped to the transport's supported range by the transport itself.
            max_connections_per_host: self.max_connections,
            connect_timeout: defaults.connect_timeout.min(self.timeout),
            read_timeout: self.timeout,
            proxy: match &self.proxy {
                ProxySetting::System => ProxyConfig::System,
                ProxySetting::Custom { url } => ProxyConfig::Manual {
                    url: url.clone(),
                    credentials: None,
                },
            },
            ..defaults
        }
    }
}

/// Adjusts the transport configuration after it was derived from the settings (tests).
type ConfigTweak = Box<dyn Fn(&mut TransportConfig) + Send + Sync>;

struct State {
    transport: Arc<HttpTransport>,
    net: NetSettings,
    /// Bumped on every rebuild; an open that started before a rebuild is not cached.
    generation: u64,
    /// Root sessions by [`url_key`] of their repository root.
    roots: HashMap<String, HttpRaSession>,
}

/// [`SessionConnector`] over one shared [`HttpTransport`] with a session cache per repository.
pub struct HttpConnector {
    prompter: Arc<dyn AuthPrompter>,
    trust: Arc<dyn TrustStore>,
    settings: Mutex<watch::Receiver<Settings>>,
    invalidations: Mutex<watch::Receiver<u64>>,
    tweak: Option<ConfigTweak>,
    state: Mutex<State>,
    /// Serializes opens per origin so concurrent operations on one repository open it once.
    open_gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `scheme://host:port` with the host in lower case.
fn origin_key(url: &RepoUrl) -> String {
    let u = url.as_url();
    format!(
        "{}://{}:{}",
        u.scheme().to_ascii_lowercase(),
        u.host_str().unwrap_or_default().to_ascii_lowercase(),
        u.port_or_known_default().unwrap_or_default()
    )
}

/// Cache key of a URL: [`origin_key`] plus the (case-sensitive) path without trailing slash.
fn url_key(url: &RepoUrl) -> String {
    format!(
        "{}{}",
        origin_key(url),
        url.as_url().path().trim_end_matches('/')
    )
}

impl HttpConnector {
    /// Connector whose transport asks `prompter` and trusts `trust`, configured from `settings`.
    /// Every change on `settings` (network keys) or `invalidations` drops the cached sessions.
    pub fn new(
        prompter: Arc<dyn AuthPrompter>,
        trust: Arc<dyn TrustStore>,
        settings: watch::Receiver<Settings>,
        invalidations: watch::Receiver<u64>,
    ) -> Self {
        Self::build(prompter, trust, settings, invalidations, None)
    }

    /// Connector over the app's broker, trust store and settings.
    pub(crate) fn from_services(services: &crate::app_state::EngineServices) -> Self {
        Self::new(
            services.auth.clone(),
            services.auth.trust_store(),
            services.settings.subscribe(),
            services.auth.invalidations(),
        )
    }

    /// Like [`HttpConnector::new`], with `tweak` applied to every transport configuration.
    #[cfg(test)]
    pub(crate) fn with_config_tweak(
        prompter: Arc<dyn AuthPrompter>,
        trust: Arc<dyn TrustStore>,
        settings: watch::Receiver<Settings>,
        invalidations: watch::Receiver<u64>,
        tweak: impl Fn(&mut TransportConfig) + Send + Sync + 'static,
    ) -> Self {
        Self::build(
            prompter,
            trust,
            settings,
            invalidations,
            Some(Box::new(tweak)),
        )
    }

    fn build(
        prompter: Arc<dyn AuthPrompter>,
        trust: Arc<dyn TrustStore>,
        mut settings: watch::Receiver<Settings>,
        mut invalidations: watch::Receiver<u64>,
        tweak: Option<ConfigTweak>,
    ) -> Self {
        let net = NetSettings::of(&settings.borrow_and_update());
        invalidations.borrow_and_update();
        let transport = make_transport(&prompter, &trust, tweak.as_ref(), &net);
        Self {
            prompter,
            trust,
            settings: Mutex::new(settings),
            invalidations: Mutex::new(invalidations),
            tweak,
            state: Mutex::new(State {
                transport,
                net,
                generation: 0,
                roots: HashMap::new(),
            }),
            open_gates: Mutex::default(),
        }
    }

    /// Applies pending settings changes and invalidations; returns the transport, its
    /// generation and the cached root session for `url`, if any.
    fn current(&self, url: &RepoUrl) -> (Arc<HttpTransport>, u64, Option<HttpRaSession>) {
        let mut state = locked(&self.state);
        let changed_net = {
            let mut rx = locked(&self.settings);
            match rx.has_changed() {
                Ok(true) => Some(NetSettings::of(&rx.borrow_and_update())),
                _ => None,
            }
        };
        let invalidated = {
            let mut rx = locked(&self.invalidations);
            let changed = matches!(rx.has_changed(), Ok(true));
            if changed {
                rx.borrow_and_update();
            }
            changed
        };
        let net_differs = changed_net.as_ref().is_some_and(|n| *n != state.net);
        if net_differs || invalidated {
            if let Some(net) = changed_net {
                state.net = net;
            }
            tracing::debug!(
                settings = net_differs,
                invalidated,
                "dropping cached repository sessions"
            );
            state.transport =
                make_transport(&self.prompter, &self.trust, self.tweak.as_ref(), &state.net);
            state.generation += 1;
            state.roots.clear();
        }
        let wanted = url_key(url);
        let cached = state
            .roots
            .iter()
            .filter(|(root, _)| {
                wanted == **root
                    || wanted
                        .strip_prefix(root.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(root, _)| root.len())
            .map(|(_, session)| session.clone());
        (Arc::clone(&state.transport), state.generation, cached)
    }

    fn gate(&self, url: &RepoUrl) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(locked(&self.open_gates).entry(origin_key(url)).or_default())
    }

    /// The root session of the repository containing `url`, opened if needed.
    async fn root_session(
        &self,
        url: &RepoUrl,
        interactive: bool,
        cancel: &CancelToken,
    ) -> Result<HttpRaSession, SvnError> {
        // An unattended session cannot become attended again: an interactive request opens anew
        // (and replaces it in the cache).
        let usable = |s: &HttpRaSession| !interactive || !s.wire().is_unattended();
        if let (_, _, Some(root)) = self.current(url)
            && usable(&root)
        {
            return Ok(root);
        }
        let gate = self.gate(url);
        let _guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(SvnError::new(ErrorCode::Cancelled, "operation cancelled"));
            }
            g = gate.lock() => g,
        };
        let (transport, generation, cached) = self.current(url);
        if let Some(root) = cached
            && usable(&root)
        {
            return Ok(root);
        }
        // Opened at the requested URL: path-based authorization may forbid the root itself.
        let opened = if interactive {
            HttpRaSession::open(transport, url.clone(), cancel).await?
        } else {
            HttpRaSession::open_unattended(transport, url.clone(), cancel).await?
        };
        let root = opened.reparent(opened.repos_root())?;
        let mut state = locked(&self.state);
        if state.generation == generation {
            state.roots.insert(url_key(root.repos_root()), root.clone());
        }
        Ok(root)
    }
}

fn make_transport(
    prompter: &Arc<dyn AuthPrompter>,
    trust: &Arc<dyn TrustStore>,
    tweak: Option<&ConfigTweak>,
    net: &NetSettings,
) -> Arc<HttpTransport> {
    let mut config = net.transport_config();
    if let Some(tweak) = tweak {
        tweak(&mut config);
    }
    let transport = HttpTransport::new(config, Arc::clone(prompter), Arc::clone(trust));
    // Off: never try Kerberos; servers that offer Basic too get the password prompt.
    Arc::new(if net.negotiate {
        transport
    } else {
        transport.with_negotiate(None)
    })
}

#[async_trait]
impl SessionConnector for HttpConnector {
    async fn connect(
        &self,
        url: &RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        let options = ConnectOptions {
            anchor: Anchor::Root,
            interactive: true,
        };
        self.connect_with(url, &options, cancel).await
    }

    async fn connect_with(
        &self,
        url: &RepoUrl,
        options: &ConnectOptions,
        cancel: &CancelToken,
    ) -> Result<Arc<dyn RaSession>, SvnError> {
        cancel.check()?;
        let root = self.root_session(url, options.interactive, cancel).await?;
        let session = match options.anchor {
            Anchor::Root => root,
            Anchor::Url => root.reparent(url)?,
        };
        let session = if options.interactive {
            session
        } else {
            session.unattended()
        };
        Ok(Arc::new(session))
    }
}
