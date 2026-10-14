//! [`HttpConnector`]: the app's [`SessionConnector`] over the HTTP transport.
//!
//! * **One transport.** Every session shares one long-lived [`HttpTransport`], so credentials,
//!   accepted certificates, Negotiate state and the "saved password already tried" bookkeeping
//!   are per app, not per operation. A transport per operation would offer a stale saved password
//!   again on every operation, and a few background checks would lock a domain account.
//! * **One open per repository.** A session is opened (one `OPTIONS`) at the URL that was asked
//!   for, so path-based authorization applies (a user allowed on `/trunk` only can still connect),
//!   and is cached under its repository root. Later requests for any URL below that root reuse it
//!   through [`HttpRaSession::reparent`], which sends nothing. Lookup is by longest root prefix
//!   on whole path segments (`/svn/repo` never serves `/svn/repo2`); scheme and host compare
//!   case-insensitively. The path compares exactly, except for a repository whose server showed
//!   it ignores case (VisualSVN on Windows: a URL spelled `/svn/repo/...` opened a repository
//!   announced as `/svn/Repo`). Below such a root, URLs match with the ASCII-only case folding
//!   `svn-protocol` uses for the same server behaviour. Folding is learned per repository, never
//!   assumed: on a case-sensitive server `/svn/Repo` and `/svn/repo` are different repositories.
//! * **Unattended opens never queue behind a prompt.** Opens are serialized per requested URL,
//!   not per server, so a sign-in prompt that waits for the user (up to minutes) does not hold
//!   the connector's open gate for other repositories of the same server. Unattended requests
//!   that would have to wait for that prompt fail with `auth-required` instead. Interactive
//!   requests to the same origin and realm do wait for it, by design: the transport shows one
//!   prompt per origin and every waiting request reuses its answer (see the transport). If the
//!   prompt is cancelled or times out, only those waiting requests fail; the next interactive
//!   one asks again.
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
    roots: HashMap<String, CachedRoot>,
}

/// A cached repository root session.
#[derive(Clone)]
struct CachedRoot {
    session: HttpRaSession,
    /// The server announced this root for a URL that spelled it in another ASCII case, so it
    /// ignores case (VisualSVN on Windows) and URLs below it match case-insensitively.
    folds_case: bool,
}

/// [`SessionConnector`] over one shared [`HttpTransport`] with a session cache per repository.
pub struct HttpConnector {
    prompter: Arc<dyn AuthPrompter>,
    trust: Arc<dyn TrustStore>,
    settings: Mutex<watch::Receiver<Settings>>,
    invalidations: Mutex<watch::Receiver<u64>>,
    tweak: Option<ConfigTweak>,
    state: Mutex<State>,
    /// Serializes opens per requested URL so concurrent operations on one location open it
    /// once, without making other repositories of the server wait (for example behind a prompt).
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

/// Whether `path` equals `prefix` or lies below it, on whole segments.
fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// [`under`] with ASCII-only case folding (as `svn-protocol` folds VisualSVN roots; full
/// Unicode folding would match distinct names).
fn under_folded(path: &str, prefix: &str) -> bool {
    path.len() >= prefix.len()
        && path.is_char_boundary(prefix.len())
        && path[..prefix.len()].eq_ignore_ascii_case(prefix)
        && (path.len() == prefix.len() || path.as_bytes()[prefix.len()] == b'/')
}

/// The key of the cached root for the URL key `wanted`, out of `(root key, folds case)` pairs:
/// the longest exact root prefix, else the longest case-folded one among roots known to ignore
/// case.
fn find_root<'a>(
    roots: impl Iterator<Item = (&'a str, bool)> + Clone,
    wanted: &str,
) -> Option<&'a str> {
    let longest = |fold: bool| {
        roots
            .clone()
            .filter(|&(root, folds_case)| {
                if fold {
                    folds_case && under_folded(wanted, root)
                } else {
                    under(wanted, root)
                }
            })
            .max_by_key(|(root, _)| root.len())
            .map(|(root, _)| root)
    };
    longest(false).or_else(|| longest(true))
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
    #[allow(dead_code)] // called from `lib.rs` once the real engine is the default (WP7)
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
        let roots = state.roots.iter().map(|(k, c)| (k.as_str(), c.folds_case));
        let cached = find_root(roots, &url_key(url))
            .and_then(|k| state.roots.get(k))
            .map(|c| c.session.clone());
        (Arc::clone(&state.transport), state.generation, cached)
    }

    fn gate(&self, url: &RepoUrl) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = locked(&self.open_gates);
        // Forget gates nobody holds or waits for, so the map does not grow with every URL.
        gates.retain(|_, g| Arc::strong_count(g) > 1);
        Arc::clone(gates.entry(url_key(url)).or_default())
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
        let (asked, root_key) = (url_key(url), url_key(root.repos_root()));
        let folds_case = !under(&asked, &root_key) && under_folded(&asked, &root_key);
        let mut state = locked(&self.state);
        if state.generation == generation {
            let entry = state.roots.entry(root_key).or_insert_with(|| CachedRoot {
                session: root.clone(),
                folds_case,
            });
            entry.session = root.clone();
            // Once the server showed it ignores case, it keeps doing so for this repository.
            entry.folds_case |= folds_case;
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{find_root, under_folded};

    const O: &str = "https://svn.example:443";

    fn key(path: &str) -> String {
        format!("{O}{path}")
    }

    #[test]
    fn a_root_never_serves_a_sibling_that_shares_its_prefix() {
        let repo = key("/svn/repo");
        let roots = [(repo.as_str(), false)];
        assert_eq!(
            find_root(roots.iter().copied(), &key("/svn/repo")),
            Some(repo.as_str())
        );
        assert_eq!(
            find_root(roots.iter().copied(), &key("/svn/repo/trunk")),
            Some(repo.as_str())
        );
        assert_eq!(find_root(roots.iter().copied(), &key("/svn/repo2")), None);
        assert_eq!(
            find_root(roots.iter().copied(), &key("/svn/repo2/trunk")),
            None
        );
        assert_eq!(find_root(roots.iter().copied(), &key("/svn/rep")), None);
        // Folding does not loosen the segment rule either.
        let folding = [(repo.as_str(), true)];
        assert_eq!(
            find_root(folding.iter().copied(), &key("/svn/REPO2/trunk")),
            None
        );
    }

    #[test]
    fn the_longest_root_wins_and_both_siblings_resolve_to_their_own_root() {
        let (repo, repo2, nested) = (key("/svn/repo"), key("/svn/repo2"), key("/svn/repo/sub"));
        let roots = [
            (repo.as_str(), false),
            (repo2.as_str(), false),
            (nested.as_str(), false),
        ];
        let find = |p: &str| find_root(roots.iter().copied(), &key(p));
        assert_eq!(find("/svn/repo2/trunk"), Some(repo2.as_str()));
        assert_eq!(find("/svn/repo/trunk"), Some(repo.as_str()));
        assert_eq!(find("/svn/repo/sub/x"), Some(nested.as_str()));
        assert_eq!(find("/svn/repo/subx"), Some(repo.as_str()));
    }

    #[test]
    fn case_differing_urls_match_only_roots_known_to_ignore_case() {
        let root = key("/svn/Repo");
        let strict = [(root.as_str(), false)];
        assert_eq!(
            find_root(strict.iter().copied(), &key("/svn/repo/trunk")),
            None
        );
        let visualsvn = [(root.as_str(), true)];
        assert_eq!(
            find_root(visualsvn.iter().copied(), &key("/svn/repo/trunk")),
            Some(root.as_str())
        );
        // An exact match is preferred over a folded one.
        let other = key("/svn/repo");
        let both = [(root.as_str(), true), (other.as_str(), false)];
        assert_eq!(
            find_root(both.iter().copied(), &key("/svn/repo/trunk")),
            Some(other.as_str())
        );
    }

    #[test]
    fn folding_is_ascii_only() {
        // KELVIN SIGN folds to `k` under Unicode rules; svn-protocol does not fold it, nor do we.
        assert!(!under_folded("/svn/\u{212A}", "/svn/k"));
        assert!(under_folded("/svn/K/x", "/svn/k"));
        assert!(!under_folded("/svn/kx", "/svn/k"));
    }
}
