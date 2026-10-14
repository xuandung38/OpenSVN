//! Per-session wiring shared by the update driver, the commit editor and remote edits.
//!
//! [`RaWire`] bundles the transport with the parsed server capabilities and the URL builder of the
//! protocol layer, converts sans-IO protocol requests into transport requests and turns failed
//! responses into [`SvnError`]s. The session type of the RA layer builds one from its `OPTIONS`
//! answer ([`RaWire::new`]) or lets [`RaWire::open`] ask the server.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use svn_protocol::{
    Body, BulkPreference, Capabilities, Headers, HrefKind, ParserLimits, UrlBuilder,
    error_from_response, options_request,
};
use svn_types::{CancelToken, ErrorCode, RelPath, RepoUrl, Result, SvnError};

use crate::ops::read::ListLimits;
use crate::transport::{BufferedResponse, HttpRequest, HttpResponse, HttpTransport, RequestBody};

/// Default number of parallel connections assumed when the caller does not say.
const DEFAULT_CONNECTIONS: usize = 4;

/// Largest answer buffered by [`RaWire::read_small`] (commit answers are tiny).
const SMALL_BODY_LIMIT: usize = 4 << 20;

/// Transport plus protocol state of one repository session.
#[derive(Debug, Clone)]
pub struct RaWire {
    transport: Arc<HttpTransport>,
    session_url: RepoUrl,
    /// Where the session was opened (its `OPTIONS` succeeded there); kept by
    /// [`Self::reparent`]. Under path-based authorization this may be the only location the
    /// user can read.
    opened_url: RepoUrl,
    caps: Arc<Capabilities>,
    urls: Arc<UrlBuilder>,
    /// Session location relative to the repository root.
    session_rel: RelPath,
    connections: usize,
    bulk: BulkPreference,
    list_limits: ListLimits,
    /// Every request goes through [`HttpTransport::send_unattended`]: no credential or
    /// certificate prompt, only what is cached, accepted or stored.
    unattended: bool,
    /// Round trip of the opening `OPTIONS` when it took a single exchange (no authentication
    /// round), the measure libsvn_ra_serf uses to call a connection low-latency.
    open_latency: Option<std::time::Duration>,
}

impl RaWire {
    /// Builds the wiring from capabilities already parsed from the session's `OPTIONS` answer.
    pub fn new(
        transport: Arc<HttpTransport>,
        session_url: RepoUrl,
        caps: Capabilities,
    ) -> Result<Self> {
        let urls = UrlBuilder::new(&session_url, &caps)?;
        let (session_url, session_rel) = locate_session(&urls, session_url)?;
        Ok(Self {
            transport,
            opened_url: session_url.clone(),
            session_url,
            caps: Arc::new(caps),
            urls: Arc::new(urls),
            session_rel,
            connections: DEFAULT_CONNECTIONS,
            bulk: BulkPreference::Auto,
            list_limits: ListLimits::default(),
            unattended: false,
            open_latency: None,
        })
    }

    /// Sends `OPTIONS` to the session URL and builds the wiring from the answer.
    pub async fn open(
        transport: Arc<HttpTransport>,
        session_url: RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Self> {
        Self::open_mode(transport, session_url, false, cancel).await
    }

    /// Like [`Self::open`], but the `OPTIONS` (and every later request of the wiring) never
    /// prompts: see [`Self::unattended`].
    pub async fn open_unattended(
        transport: Arc<HttpTransport>,
        session_url: RepoUrl,
        cancel: &CancelToken,
    ) -> Result<Self> {
        Self::open_mode(transport, session_url, true, cancel).await
    }

    async fn open_mode(
        transport: Arc<HttpTransport>,
        session_url: RepoUrl,
        unattended: bool,
        cancel: &CancelToken,
    ) -> Result<Self> {
        let req = options_request(&session_url)?;
        let body = buffered_body(&req)?;
        let started = std::time::Instant::now();
        let resp = send_with(&transport, req, body, unattended, cancel).await?;
        let elapsed = started.elapsed();
        let head = svn_protocol::ResponseHead::new(resp.status().as_u16(), headers_of(&resp));
        let body = read_limited(resp, ParserLimits::default().max_error_body_bytes, cancel).await?;
        // mod_dav_svn answers a bare 500 (no `D:error`) on an `SVNParentPath` location.
        if head.status == 500
            && svn_protocol::errors::parse_dav_error(&body).is_none()
            && let Some(err) = parent_path_check(&transport, &session_url, unattended, cancel).await
        {
            return Err(err);
        }
        let caps = Capabilities::from_options_response(&session_url, &head, &body)?;
        let mut wire = Self::new(transport, session_url, caps)?;
        wire.unattended = unattended;
        // A prompt or an authentication round makes the time meaningless; it then just
        // counts as not low-latency.
        wire.open_latency = Some(elapsed);
        Ok(wire)
    }

    /// The same wiring rooted at `url`, without a request: transport, capabilities and URL
    /// builder are shared. `url` must be on the session's origin and at or below the
    /// repository root (a case-only difference in the root part is accepted and replaced by
    /// the server's spelling, see [`UrlBuilder::locate`]); anything else is
    /// [`ErrorCode::UnsafePath`].
    pub fn reparent(&self, url: &RepoUrl) -> Result<Self> {
        let (session_url, session_rel) = locate_session(&self.urls, url.clone())?;
        Ok(Self {
            session_url,
            session_rel,
            ..self.clone()
        })
    }

    /// The same wiring with every request sent through [`HttpTransport::send_unattended`]:
    /// nothing ever prompts; only credentials already cached for the origin or returned by
    /// [`AuthPrompter::stored_credentials`](svn_ra_api::AuthPrompter::stored_credentials) and
    /// certificates already accepted are used. A request that would need a prompt fails with
    /// `AuthRequired` (or its TLS error).
    #[must_use]
    pub fn unattended(mut self) -> Self {
        self.unattended = true;
        self
    }

    /// Whether the connection looks low-latency the way libsvn_ra_serf decides it (the opening
    /// `OPTIONS` answered within 5 ms, which includes connecting; any authentication round or
    /// prompt makes it slower). Commits then send svndiff 2 (LZ4, cheap to compress and to apply)
    /// instead of svndiff 1 (zlib, smaller on a slow link), like svn.
    pub fn is_low_latency(&self) -> bool {
        self.open_latency
            .is_some_and(|d| d < std::time::Duration::from_millis(5))
    }

    /// Whether requests never prompt (see [`Self::unattended`]).
    pub fn is_unattended(&self) -> bool {
        self.unattended
    }

    /// Sets the number of connections the transport pool offers (clamped to 1..=8). The update
    /// driver keeps one for the report itself and uses the rest for parallel downloads.
    #[must_use]
    pub fn with_connections(mut self, connections: usize) -> Self {
        self.connections = connections.clamp(1, 8);
        self
    }

    /// Chooses between send-all and skeleton updates (default: let the server decide).
    #[must_use]
    pub fn with_bulk_preference(mut self, bulk: BulkPreference) -> Self {
        self.bulk = bulk;
        self
    }

    /// Caps for recursive (`Depth::Infinity`) listings.
    #[must_use]
    pub fn with_list_limits(mut self, limits: ListLimits) -> Self {
        self.list_limits = limits;
        self
    }

    /// Caps for recursive listings.
    pub fn list_limits(&self) -> ListLimits {
        self.list_limits
    }

    /// Update mode preference.
    pub fn bulk_preference(&self) -> BulkPreference {
        self.bulk
    }

    /// Connections the transport pool offers (1..=8).
    pub fn connections(&self) -> usize {
        self.connections
    }

    /// Parsed server capabilities.
    pub fn caps(&self) -> &Capabilities {
        &self.caps
    }

    /// URL builder for the stubs this server announced.
    pub fn urls(&self) -> &UrlBuilder {
        &self.urls
    }

    /// The transport.
    pub fn transport(&self) -> &Arc<HttpTransport> {
        &self.transport
    }

    /// URL the session is rooted at.
    pub fn session_url(&self) -> &RepoUrl {
        &self.session_url
    }

    /// URL the session was opened at (before any [`Self::reparent`]).
    pub fn opened_url(&self) -> &RepoUrl {
        &self.opened_url
    }

    /// Session location relative to the repository root.
    pub fn session_rel(&self) -> &RelPath {
        &self.session_rel
    }

    /// Repository-relative path of a session-relative one.
    pub fn repo_path(&self, rel: &RelPath) -> Result<RelPath> {
        join_rel(&self.session_rel, rel)
    }

    /// Repository-relative path of an absolute URL (or absolute server path) that lies below the
    /// repository root of this server. Anything else is [`ErrorCode::UnsafePath`].
    pub fn repo_path_of_url(&self, url: &str) -> Result<RelPath> {
        match self.urls.accept_href(url)?.kind {
            HrefKind::Public { path } => Ok(path),
            _ => Err(SvnError::new(
                ErrorCode::UnsafePath,
                "URL does not name a node of the repository",
            )),
        }
    }

    /// Sends a protocol request (buffered or empty body).
    pub async fn send(
        &self,
        req: svn_protocol::HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse> {
        let body = buffered_body(&req)?;
        send_with(&self.transport, req, body, self.unattended, cancel).await
    }

    /// Sends a protocol request, replacing its body with `body` (replayable uploads).
    pub async fn send_with_body(
        &self,
        req: svn_protocol::HttpRequest,
        body: RequestBody,
        cancel: &CancelToken,
    ) -> Result<HttpResponse> {
        send_with(&self.transport, req, body, self.unattended, cancel).await
    }

    /// Sends a protocol request without ever prompting (see
    /// [`HttpTransport::send_unattended`]); for best-effort background cleanup.
    pub async fn send_unattended(
        &self,
        req: svn_protocol::HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse> {
        let body = buffered_body(&req)?;
        cancel.check()?;
        self.transport
            .send_unattended(convert(req, body)?, cancel)
            .await
    }

    /// Sends a protocol request (buffered or empty body) and reads the whole answer (at most
    /// 4 MiB). The answer of a read-only request that breaks off on a failed connection is
    /// fetched again on a fresh connection (see [`HttpTransport::send_and_read`]).
    pub async fn fetch_small(
        &self,
        req: svn_protocol::HttpRequest,
        cancel: &CancelToken,
    ) -> Result<BufferedResponse> {
        let body = buffered_body(&req)?;
        cancel.check()?;
        let req = convert(req, body)?;
        let fetch = async {
            if self.unattended {
                self.transport
                    .send_and_read_unattended(req, SMALL_BODY_LIMIT, cancel)
                    .await
            } else {
                self.transport
                    .send_and_read(req, SMALL_BODY_LIMIT, cancel)
                    .await
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(SvnError::new(ErrorCode::Cancelled, "operation cancelled")),
            r = fetch => r,
        }
    }

    /// [`Self::fetch_small`] that fails with the server's error unless the status is 2xx.
    pub async fn fetch_small_ok(
        &self,
        req: svn_protocol::HttpRequest,
        cancel: &CancelToken,
    ) -> Result<Bytes> {
        let resp = self.fetch_small(req, cancel).await?;
        if resp.status.is_success() {
            Ok(resp.body)
        } else {
            Err(buffered_error(&resp))
        }
    }

    /// Sends a request and fails with the server's error unless the status is 2xx.
    pub async fn send_ok(
        &self,
        req: svn_protocol::HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse> {
        let resp = self.send(req, cancel).await?;
        Self::require_success(resp, cancel).await
    }

    /// Passes a 2xx response through; turns anything else into the server's error.
    pub async fn require_success(resp: HttpResponse, cancel: &CancelToken) -> Result<HttpResponse> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        Err(error_of(resp, cancel).await)
    }

    /// Reads a small answer completely (at most 4 MiB). Fails with `Cancelled` as soon as
    /// `cancel` fires (as well as when the token the request was sent with fires).
    pub async fn read_small(resp: HttpResponse, cancel: &CancelToken) -> Result<Bytes> {
        cancel.check()?;
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(SvnError::new(ErrorCode::Cancelled, "operation cancelled")),
            r = resp.bytes(SMALL_BODY_LIMIT) => r,
        }
    }
}

/// Largest listing page read when telling a repository list from a server error.
const PARENT_LISTING_LIMIT: usize = 256 << 10;

/// When `url` is the repository list of an `SVNParentPath` location (VisualSVN Server's
/// `https://host/svn/`), the error that says so and names the repositories. `None` for any
/// other answer: the caller then reports the original failure. Cancellation is reported.
async fn parent_path_check(
    transport: &HttpTransport,
    url: &RepoUrl,
    unattended: bool,
    cancel: &CancelToken,
) -> Option<SvnError> {
    let req = svn_protocol::parent_listing_request(url).ok()?;
    let body = buffered_body(&req).ok()?;
    let resp = match send_with(transport, req, body, unattended, cancel).await {
        Ok(resp) => resp,
        Err(err) if err.code == ErrorCode::Cancelled => return Some(err),
        Err(_) => return None,
    };
    if !resp.status().is_success() {
        return None;
    }
    let page = match read_limited(resp, PARENT_LISTING_LIMIT, cancel).await {
        Ok(page) => page,
        Err(err) if err.code == ErrorCode::Cancelled => return Some(err),
        Err(_) => return None,
    };
    let names = svn_protocol::parent_path_listing(&page)?;
    Some(svn_protocol::parent_path_error(url, &names))
}

/// Transport body for a protocol request that carries no stream.
fn buffered_body(req: &svn_protocol::HttpRequest) -> Result<RequestBody> {
    match &req.body {
        Body::Empty => Ok(RequestBody::Empty),
        Body::Bytes(b) => Ok(RequestBody::Bytes(b.clone())),
        Body::Stream(_) => Err(SvnError::new(
            ErrorCode::Internal,
            "streamed protocol bodies must be sent with send_with_body",
        )),
    }
}

/// Where `url` lies below the repository root: the session URL to use (the server's spelling
/// of the root when only its case differed) and its repository-relative path. Fails with
/// `UnsafePath` for another origin, a URL outside the root or one inside a special resource.
fn locate_session(urls: &UrlBuilder, url: RepoUrl) -> Result<(RepoUrl, RelPath)> {
    let (rel, folded) = urls.locate(url.as_url())?;
    let url = if folded {
        RepoUrl::parse(urls.public(&rel)?.as_str())?
    } else {
        url
    };
    match urls.accept_href(url.as_url().as_str())?.kind {
        HrefKind::Public { path } => Ok((url, path)),
        _ => Err(SvnError::new(
            ErrorCode::UnsafePath,
            "session URL points into a special Subversion resource",
        )),
    }
}

/// Joins two relative paths (`base` + `rel`), validating the result.
pub(crate) fn join_rel(base: &RelPath, rel: &RelPath) -> Result<RelPath> {
    if base.is_root() {
        Ok(rel.clone())
    } else if rel.is_root() {
        Ok(base.clone())
    } else {
        RelPath::new(&format!("{base}/{rel}"))
    }
}

/// Converts the headers of a transport response into the protocol layer's header list.
pub(crate) fn headers_of(resp: &HttpResponse) -> Headers {
    protocol_headers(resp.headers())
}

/// [`headers_of`] for an answer read with [`RaWire::fetch_small`].
pub(crate) fn buffered_headers(resp: &BufferedResponse) -> Headers {
    protocol_headers(&resp.headers)
}

fn protocol_headers(headers: &HeaderMap) -> Headers {
    Headers::from_pairs(headers.iter().filter_map(|(n, v)| {
        v.to_str()
            .ok()
            .map(|v| (n.as_str().to_owned(), v.to_owned()))
    }))
}

/// Reads at most `limit` bytes of the body and drops the rest (error bodies).
pub(crate) async fn read_limited(
    mut resp: HttpResponse,
    limit: usize,
    cancel: &CancelToken,
) -> Result<Bytes> {
    cancel.check()?;
    let mut buf = BytesMut::new();
    while buf.len() < limit {
        match resp.chunk().await? {
            Some(c) => {
                let take = c.len().min(limit - buf.len());
                buf.extend_from_slice(&c[..take]);
            }
            None => break,
        }
    }
    Ok(buf.freeze())
}

/// Error for a non-success answer read with [`RaWire::fetch_small`].
pub(crate) fn buffered_error(resp: &BufferedResponse) -> SvnError {
    let headers = buffered_headers(resp);
    let limit = ParserLimits::default().max_error_body_bytes;
    let body = &resp.body[..resp.body.len().min(limit)];
    error_from_response(resp.status.as_u16(), &headers, body)
}

/// Error for a non-success response: the server's `D:error` chain or a status-based fallback.
pub(crate) async fn error_of(resp: HttpResponse, cancel: &CancelToken) -> SvnError {
    let status = resp.status().as_u16();
    let headers = headers_of(&resp);
    // A body that cannot be read must not hide the status.
    let body = read_limited(resp, ParserLimits::default().max_error_body_bytes, cancel)
        .await
        .unwrap_or_default();
    error_from_response(status, &headers, &body)
}

fn convert(req: svn_protocol::HttpRequest, body: RequestBody) -> Result<HttpRequest> {
    let method = reqwest::Method::from_bytes(req.method.as_str().as_bytes())
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("invalid HTTP method: {e}")))?;
    let mut headers = HeaderMap::new();
    for (name, value) in req.headers.iter() {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| SvnError::new(ErrorCode::Internal, format!("invalid header name: {e}")))?;
        // Values can carry server-derived text (lock tokens); a bad one must not be sent.
        let value = HeaderValue::from_str(value).map_err(|e| {
            SvnError::new(
                ErrorCode::Internal,
                format!("invalid value for header {name}: {e}"),
            )
        })?;
        headers.append(name, value);
    }
    Ok(HttpRequest {
        method,
        url: req.url,
        headers,
        body,
    })
}

async fn send_with(
    transport: &HttpTransport,
    req: svn_protocol::HttpRequest,
    body: RequestBody,
    unattended: bool,
    cancel: &CancelToken,
) -> Result<HttpResponse> {
    cancel.check()?;
    let req = convert(req, body)?;
    if unattended {
        transport.send_unattended(req, cancel).await
    } else {
        transport.send(req, cancel).await
    }
}
