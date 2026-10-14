//! `HttpTransport`: reqwest client per origin with TLS policy, auth, retry and redirects.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH, LOCATION};
use reqwest::{Method, StatusCode};
use svn_ra_api::{AuthPrompter, CertDecision, CertFailure};
use svn_types::{CancelToken, ErrorCode, Result, SvnError};
use tokio::sync::Semaphore;
use url::Url;

use super::config::{ProxyConfig, TransportConfig};
use super::redact::{RedactedHeaders, redacted_url};
use super::request::{HttpRequest, HttpResponse};
use super::retry::{classify_error, error_chain, is_idempotent, is_transient_status};
use super::tls::{
    CertProblem, OriginTls, PinAction, TrustStore, make_client_config, tls_server_end_point,
};
use crate::auth::{Attached, AuthManager, Challenges, HostKey, NegotiateProvider, or_cancel};

/// How many certificate prompts one request may trigger before giving up.
const MAX_TLS_ROUNDS: u8 = 2;
/// 401 responses tolerated per realm: the unauthenticated request plus three rejected submissions.
const MAX_CHALLENGES_PER_REALM: u8 = crate::auth::MAX_ATTEMPTS + 1;
/// Total 401 round trips tolerated per request, whatever the realms.
const MAX_AUTH_ROUND_TRIPS: u8 = 10;

/// Client, TLS state and connection limit of one `(scheme, host, port)`.
struct OriginClient {
    client: reqwest::Client,
    tls: Arc<OriginTls>,
    slots: Arc<Semaphore>,
    /// RFC 5929 `tls-server-end-point` channel binding of the certificate the server presented on
    /// its latest response, for Negotiate (Extended Protection).
    channel_binding: Mutex<Option<Arc<[u8]>>>,
}

impl OriginClient {
    fn channel_binding(&self) -> Option<Arc<[u8]>> {
        self.channel_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Remembers the binding for the certificate the response came with.
    fn record_certificate(&self, resp: &reqwest::Response) {
        let Some(der) = resp
            .extensions()
            .get::<reqwest::tls::TlsInfo>()
            .and_then(reqwest::tls::TlsInfo::peer_certificate)
        else {
            return;
        };
        let binding = tls_server_end_point(der).map(Arc::from);
        *self
            .channel_binding
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = binding;
    }
}

/// Why a single attempt failed.
enum AttemptError {
    /// Certificate rejected by the verifier; may be resolved by the user.
    Tls(Box<CertProblem>),
    Other(SvnError),
}

impl From<SvnError> for AttemptError {
    fn from(e: SvnError) -> Self {
        Self::Other(e)
    }
}

/// Whether a send may ask the [`AuthPrompter`] for credentials or a certificate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interaction {
    Allowed,
    Forbidden,
}

/// HTTP/1.1 transport with a bounded connection pool per origin.
pub struct HttpTransport {
    config: TransportConfig,
    prompter: Arc<dyn AuthPrompter>,
    trust: Arc<dyn TrustStore>,
    auth: AuthManager,
    origins: Mutex<HashMap<HostKey, Arc<OriginClient>>>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport").finish_non_exhaustive()
    }
}

impl HttpTransport {
    /// Creates a transport. `prompter` supplies credentials and certificate decisions;
    /// `trust` holds the pinned certificate fingerprints.
    pub fn new(
        config: TransportConfig,
        prompter: Arc<dyn AuthPrompter>,
        trust: Arc<dyn TrustStore>,
    ) -> Self {
        Self {
            auth: AuthManager::new(
                Arc::clone(&prompter),
                crate::auth::negotiate::system_provider(),
            ),
            config,
            prompter,
            trust,
            origins: Mutex::default(),
        }
    }

    /// Replaces the Negotiate (Kerberos SSO) provider; `None` disables Negotiate. By default the
    /// platform provider is used when the build has one (see [`crate::auth::negotiate`]).
    #[must_use]
    pub fn with_negotiate(mut self, provider: Option<Arc<dyn NegotiateProvider>>) -> Self {
        self.auth.set_negotiate(provider);
        self
    }

    /// Changes how long a Negotiate failure is remembered per host before single sign-on is
    /// tried again (default [`crate::auth::NEGOTIATE_RETRY_AFTER`]).
    #[must_use]
    pub fn with_negotiate_retry_after(mut self, after: Duration) -> Self {
        self.auth.set_negotiate_retry_after(after);
        self
    }

    /// Sends a request and returns the response once its headers arrive.
    ///
    /// Handles 401 challenges (prompting through the `AuthPrompter`), transient failures of
    /// idempotent methods, TLS exceptions and same-origin redirects. Statuses other than those
    /// are returned for the caller to interpret; see [`super::classify_status`].
    pub async fn send(&self, req: HttpRequest, cancel: &CancelToken) -> Result<HttpResponse> {
        self.send_mode(req, cancel, Interaction::Allowed).await
    }

    /// Like [`Self::send`], but never asks the user: only credentials already cached for the
    /// origin, credentials the prompter has stored ([`AuthPrompter::stored_credentials`],
    /// which shows no UI), Negotiate with an existing ticket and certificates already accepted
    /// are used. A 401 those do not satisfy fails with `AuthRequired`, and a certificate that
    /// would need a decision fails with its TLS error.
    ///
    /// Meant for background work (remote status, deleting an abandoned commit transaction)
    /// that must not pop up a prompt after the user has moved on.
    pub async fn send_unattended(
        &self,
        req: HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse> {
        self.send_mode(req, cancel, Interaction::Forbidden).await
    }

    async fn send_mode(
        &self,
        mut req: HttpRequest,
        cancel: &CancelToken,
        interaction: Interaction,
    ) -> Result<HttpResponse> {
        validate_url(&req.url)?;
        req.headers.remove(AUTHORIZATION);
        let mut redirects = 0u8;
        loop {
            let origin = self.origin_client(&req.url)?;
            let resp = self
                .send_authenticated(&origin, &mut req, cancel, interaction)
                .await?;
            if !is_redirect(resp.status) {
                return Ok(resp);
            }
            redirects += 1;
            if redirects > self.config.max_redirects {
                return Err(SvnError::new(ErrorCode::Network, "too many redirects"));
            }
            let status = resp.status;
            let target = redirect_target(&req.url, &resp)?;
            drop(resp);
            if !follows_redirects(&req.method) {
                return Err(SvnError::new(
                    ErrorCode::Network,
                    format!(
                        "repository moved: {} was redirected (HTTP {}) to {}; \
                         write requests are not re-sent",
                        req.method,
                        status.as_u16(),
                        redacted_url(&target)
                    ),
                ));
            }
            if !req.body.is_replayable() {
                return Err(SvnError::new(
                    ErrorCode::Network,
                    "server redirected a request whose body cannot be replayed",
                ));
            }
            if status == StatusCode::SEE_OTHER {
                req.method = Method::GET;
                req.body = super::request::RequestBody::Empty;
            }
            req.url = target;
        }
    }

    fn origin_client(&self, url: &Url) -> Result<Arc<OriginClient>> {
        let key = HostKey::from_url(url)?;
        let mut map = self.origins.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(o) = map.get(&key) {
            return Ok(Arc::clone(o));
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let port = url.port_or_known_default().unwrap_or(0);
        let tls = Arc::new(OriginTls::new(host, port, Arc::clone(&self.trust)));
        let client = self.make_client(&tls)?;
        let entry = Arc::new(OriginClient {
            client,
            tls,
            slots: Arc::new(Semaphore::new(self.config.effective_connections())),
            channel_binding: Mutex::default(),
        });
        map.insert(key, Arc::clone(&entry));
        Ok(entry)
    }

    fn make_client(&self, tls: &Arc<OriginTls>) -> Result<reqwest::Client> {
        let tls_config = make_client_config(Arc::clone(tls), &self.config.extra_root_certs)?;
        let mut b = reqwest::Client::builder()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.config.connect_timeout)
            .read_timeout(self.config.read_timeout)
            .pool_max_idle_per_host(self.config.effective_connections())
            .user_agent(self.config.user_agent.clone())
            // Exposes the server certificate per response, for Negotiate channel bindings.
            .tls_info(true)
            .use_preconfigured_tls(tls_config);
        match &self.config.proxy {
            ProxyConfig::System => {}
            ProxyConfig::Disabled => b = b.no_proxy(),
            ProxyConfig::Manual { url, credentials } => {
                let mut proxy = reqwest::Proxy::all(url).map_err(|e| {
                    SvnError::new(
                        ErrorCode::Internal,
                        format!("invalid proxy URL: {}", error_chain(&e.without_url())),
                    )
                })?;
                if let Some((user, pass)) = credentials {
                    use secrecy::ExposeSecret as _;
                    proxy = proxy.basic_auth(user, pass.expose_secret());
                }
                b = b.proxy(proxy);
            }
        }
        b.build().map_err(|e| {
            SvnError::new(
                ErrorCode::Internal,
                format!(
                    "cannot create HTTP client: {}",
                    error_chain(&e.without_url())
                ),
            )
        })
    }

    /// One logical send: loops over authentication and certificate rounds.
    async fn send_authenticated(
        &self,
        origin: &Arc<OriginClient>,
        req: &mut HttpRequest,
        cancel: &CancelToken,
        interaction: Interaction,
    ) -> Result<HttpResponse> {
        let mut sent = self
            .auth
            .prepare(&req.url, origin.channel_binding(), cancel)
            .await?;
        let mut tls_rounds = 0u8;
        let mut round_trips = 0u8;
        let mut per_realm: HashMap<String, u8> = HashMap::new();
        loop {
            cancel.check()?;
            match self.attempt(origin, req, &mut sent, cancel).await {
                Ok(resp) if resp.status == StatusCode::UNAUTHORIZED => {
                    let challenges = Challenges::from_headers(&resp.headers);
                    drop(resp);
                    // A misbehaving server must not keep the client authenticating forever.
                    round_trips += 1;
                    let realm_count = per_realm
                        .entry(challenges.basic_realm.clone().unwrap_or_default())
                        .or_default();
                    // A rejected Negotiate token does not use up one of the Basic attempts.
                    if !sent.is_negotiate() {
                        *realm_count += 1;
                    }
                    if round_trips > MAX_AUTH_ROUND_TRIPS || *realm_count > MAX_CHALLENGES_PER_REALM
                    {
                        return Err(SvnError::new(
                            ErrorCode::AuthRequired,
                            "server kept rejecting credentials; giving up",
                        ));
                    }
                    if !req.body.is_replayable() {
                        return Err(SvnError::new(
                            ErrorCode::AuthRequired,
                            "server requested authentication for a one-shot request body; \
                             authenticate with a small request first",
                        ));
                    }
                    sent = self
                        .auth
                        .on_unauthorized(
                            &req.url,
                            &challenges,
                            &sent,
                            origin.channel_binding(),
                            interaction == Interaction::Allowed,
                            cancel,
                        )
                        .await?;
                }
                Ok(resp) => {
                    self.auth
                        .on_success(&req.url, &sent, resp.status, &resp.headers, cancel)
                        .await?;
                    origin.tls.connected();
                    return Ok(resp);
                }
                Err(AttemptError::Tls(problem)) => {
                    tls_rounds += 1;
                    if tls_rounds > MAX_TLS_ROUNDS
                        || !req.body.is_replayable()
                        || interaction == Interaction::Forbidden
                    {
                        return Err(problem.to_error());
                    }
                    self.resolve_certificate(origin, *problem, cancel).await?;
                }
                Err(AttemptError::Other(e)) => return Err(e),
            }
        }
    }

    /// Asks the user about a rejected certificate when the policy allows it.
    async fn resolve_certificate(
        &self,
        origin: &OriginClient,
        problem: CertProblem,
        cancel: &CancelToken,
    ) -> Result<()> {
        if !problem.overridable {
            return Err(problem.to_error());
        }
        let tls = &origin.tls;
        let fp = problem.info.sha256;
        let _gate = or_cancel(cancel, tls.prompt_gate.lock()).await?;
        if tls.is_trusted(&fp) {
            return Ok(());
        }
        if tls.was_rejected(&fp) {
            return Err(problem.to_error());
        }
        let mut info = problem.info.clone();
        if problem.changed {
            // Only overridable failures reach this point, so `Changed` never masks a hard one.
            info.failure = CertFailure::Changed {
                old_sha256: problem.previous.last().copied().unwrap_or([0; 32]),
                new_sha256: fp,
            };
        }
        let decision = tokio::select! {
            biased;
            () = cancel.cancelled() => return cancel.check(),
            d = self.prompter.confirm_certificate(&info) => d,
        };
        // The prompter is the UI, not a trust boundary: an invalid decision for this exact
        // failure (for example AcceptOnce for a changed certificate) counts as a rejection.
        if !decision.is_valid_for(&info.failure) {
            tls.reject(fp);
            return Err(problem.to_error());
        }
        match decision {
            CertDecision::AcceptOnce => {
                tls.accept(fp, PinAction::SessionOnly);
                Ok(())
            }
            CertDecision::AcceptAlways => {
                tls.accept(fp, PinAction::Add);
                Ok(())
            }
            // Valid only for `Changed`: the user confirmed the new fingerprint.
            CertDecision::ReplacePin => {
                tls.accept(fp, PinAction::Replace);
                Ok(())
            }
            // `Reject` and any decision added later.
            _ => {
                tls.reject(fp);
                Err(problem.to_error())
            }
        }
    }

    /// Sends once (plus transient retries for idempotent requests). A retry never resends a
    /// Negotiate token: `sent` is renewed with a fresh context first.
    async fn attempt(
        &self,
        origin: &OriginClient,
        req: &mut HttpRequest,
        sent: &mut Attached,
        cancel: &CancelToken,
    ) -> std::result::Result<HttpResponse, AttemptError> {
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(cancel_error(cancel)),
            p = Arc::clone(&origin.slots).acquire_owned() => p.map_err(|_| {
                SvnError::new(ErrorCode::Internal, "connection pool closed")
            })?,
        };
        let retriable = is_idempotent(&req.method) && req.body.is_replayable();
        let mut retries = 0usize;
        loop {
            cancel.check()?;
            if retries > 0 && sent.is_negotiate() {
                *sent = self
                    .auth
                    .renew(&req.url, sent, origin.channel_binding(), cancel)
                    .await?;
            }
            let authorization = &sent.header;
            let prepared = req.body.prepare().await?;
            let mut rb = origin
                .client
                .request(req.method.clone(), req.url.clone())
                .headers(req.headers.clone());
            if let Some(h) = authorization {
                rb = rb.header(AUTHORIZATION, h.clone());
            }
            if let Some(len) = prepared.content_length {
                rb = rb.header(CONTENT_LENGTH, len);
            }
            if let Some(body) = prepared.body {
                rb = rb.body(body);
            }
            tracing::debug!(
                method = %req.method,
                url = %redacted_url(&req.url),
                headers = %RedactedHeaders(&req.headers),
                authorized = authorization.is_some(),
                "http request"
            );
            let started = Instant::now();
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(cancel_error(cancel)),
                r = rb.send() => r,
            };
            let delay = self.config.retry.delays.get(retries).copied();
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    tracing::debug!(status = status.as_u16(), "http response");
                    origin.record_certificate(&resp);
                    if retriable
                        && is_transient_status(status.as_u16())
                        && let Some(d) = delay
                    {
                        drop(resp);
                        retries += 1;
                        backoff(d, cancel).await?;
                        continue;
                    }
                    return Ok(HttpResponse {
                        status,
                        headers: resp.headers().clone(),
                        url: resp.url().clone(),
                        inner: resp,
                        cancel: cancel.clone(),
                        _permit: Some(permit),
                    });
                }
                Err(e) => {
                    if (e.is_connect() || e.is_request())
                        && let Some(p) = origin.tls.problem_since(started)
                    {
                        return Err(AttemptError::Tls(Box::new(p)));
                    }
                    if retriable
                        && !e.is_builder()
                        && let Some(d) = delay
                    {
                        retries += 1;
                        tracing::debug!(
                            error = %error_chain(&e.without_url()),
                            "transient failure, retrying"
                        );
                        backoff(d, cancel).await?;
                        continue;
                    }
                    return Err(AttemptError::Other(classify_error(e)));
                }
            }
        }
    }
}

fn cancel_error(cancel: &CancelToken) -> AttemptError {
    match cancel.check() {
        Err(e) => AttemptError::Other(e),
        Ok(()) => AttemptError::Other(SvnError::new(ErrorCode::Cancelled, "operation cancelled")),
    }
}

/// Sleeps `delay` unless cancelled first.
async fn backoff(delay: Duration, cancel: &CancelToken) -> std::result::Result<(), SvnError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => cancel.check(),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

fn validate_url(url: &Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SvnError::new(
            ErrorCode::ServerUnsupported,
            "only http and https URLs with a host are supported",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        // reqwest would turn userinfo into a Basic header, even over plain http.
        return Err(SvnError::new(
            ErrorCode::Internal,
            "credentials in the URL are not allowed",
        ));
    }
    Ok(())
}

/// Redirects are followed for read-only methods only; a moved repository is reported for
/// writes, like svn does.
fn follows_redirects(method: &Method) -> bool {
    matches!(method.as_str(), "GET" | "PROPFIND" | "OPTIONS" | "REPORT")
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// Resolves `Location` and enforces the same-origin rule.
fn redirect_target(current: &Url, resp: &HttpResponse) -> Result<Url> {
    let location = resp
        .headers
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| SvnError::new(ErrorCode::Network, "redirect without a Location header"))?;
    // The URL parser deletes tab, CR and LF and reads `\` as `/`; refuse such text outright
    // instead of following whatever it normalises to.
    svn_protocol::check_server_url_text("redirect Location", location)?;
    let target = current.join(location).map_err(|_| {
        SvnError::new(
            ErrorCode::Network,
            "redirect with an invalid Location header",
        )
    })?;
    validate_url(&target)?;
    let same_origin = target.scheme() == current.scheme()
        && target.host_str().map(str::to_ascii_lowercase)
            == current.host_str().map(str::to_ascii_lowercase)
        && target.port_or_known_default() == current.port_or_known_default();
    if !same_origin {
        return Err(SvnError::new(
            ErrorCode::Network,
            format!(
                "refusing redirect to a different origin ({})",
                redacted_url(&target)
            ),
        ));
    }
    Ok(target)
}
