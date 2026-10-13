//! Request and response types with streaming bodies in both directions.

use std::io;
use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use svn_ra_api::ReplayableSource;
use svn_types::{CancelToken, ErrorCode, Result, SvnError};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;
use url::Url;

use super::retry::classify_error;

/// Read chunk size used when streaming files.
const FILE_CHUNK: usize = 64 * 1024;

/// One-shot upload stream.
pub type BodyStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send + Sync + 'static>>;

/// Request payload.
pub enum RequestBody {
    /// No body.
    Empty,
    /// In-memory body; can be sent again after a 401, redirect or network error.
    Bytes(Bytes),
    /// File that is re-opened for every attempt; streamed with a `Content-Length`.
    File(ReplayableSource),
    /// One-shot stream. It cannot be replayed, so the transport never retries or re-sends it:
    /// authenticate (for example with an `OPTIONS`) before starting such an upload.
    Stream {
        /// Chunks to upload; consumed by the first attempt.
        stream: Option<BodyStream>,
        /// Total length when known (sent as `Content-Length`); `None` means chunked encoding.
        len: Option<u64>,
    },
}

impl std::fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Self::File(s) => write!(f, "File({:?})", s.path),
            Self::Stream { len, .. } => write!(f, "Stream(len={len:?})"),
        }
    }
}

/// Body ready to hand to reqwest.
pub(crate) struct PreparedBody {
    pub(crate) body: Option<reqwest::Body>,
    /// Length to advertise explicitly (streams only; reqwest knows the length of `Bytes`).
    pub(crate) content_length: Option<u64>,
}

impl RequestBody {
    /// Whether the body can be produced again for a retry, redirect or authenticated resend.
    pub fn is_replayable(&self) -> bool {
        !matches!(self, Self::Stream { .. })
    }

    /// Produces the body for one attempt.
    pub(crate) async fn prepare(&mut self) -> Result<PreparedBody> {
        match self {
            Self::Empty => Ok(PreparedBody {
                body: None,
                content_length: None,
            }),
            Self::Bytes(b) => Ok(PreparedBody {
                body: Some(reqwest::Body::from(b.clone())),
                content_length: None,
            }),
            Self::File(src) => {
                let file = src
                    .open()
                    .map_err(|e| SvnError::from_io("cannot open upload source", &e))?;
                let len = match src.size {
                    Some(n) => n,
                    None => file
                        .metadata()
                        .map_err(|e| SvnError::from_io("cannot stat upload source", &e))?
                        .len(),
                };
                let stream =
                    ReaderStream::with_capacity(tokio::fs::File::from_std(file), FILE_CHUNK);
                Ok(PreparedBody {
                    body: Some(reqwest::Body::wrap_stream(stream)),
                    content_length: Some(len),
                })
            }
            Self::Stream { stream, len } => {
                let s = stream.take().ok_or_else(|| {
                    SvnError::new(
                        ErrorCode::Network,
                        "one-shot request body was already consumed and cannot be re-sent",
                    )
                })?;
                Ok(PreparedBody {
                    body: Some(reqwest::Body::wrap_stream(s)),
                    content_length: *len,
                })
            }
        }
    }
}

/// An HTTP request. Custom methods (`PROPFIND`, `REPORT`, `MKCOL`, `MERGE`, `LOCK`, ...) are
/// plain [`Method`] values created with `Method::from_bytes`.
#[derive(Debug)]
pub struct HttpRequest {
    /// Request method.
    pub method: Method,
    /// Absolute target URL; must not contain userinfo.
    pub url: Url,
    /// Request headers. `Authorization` is managed by the transport.
    pub headers: HeaderMap,
    /// Payload.
    pub body: RequestBody,
}

impl HttpRequest {
    /// Request without a body.
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HeaderMap::new(),
            body: RequestBody::Empty,
        }
    }
}

/// Response with a streaming body. Dropping it closes the stream and releases the
/// connection-pool slot it occupies.
pub struct HttpResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) url: Url,
    pub(crate) inner: reqwest::Response,
    pub(crate) cancel: CancelToken,
    /// Connection slot; released as soon as the body ends or the response is dropped.
    pub(crate) _permit: Option<OwnedSemaphorePermit>,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    /// Status code.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Response headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Final URL after same-origin redirects.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Advertised body length, if any.
    pub fn content_length(&self) -> Option<u64> {
        self.inner.content_length()
    }

    /// Next body chunk, `None` at the end. Fails with `Cancelled` as soon as the request's
    /// [`CancelToken`] fires, even while waiting for the network.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        self.cancel.check()?;
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => self.cancel.check().map(|()| None),
            r = self.inner.chunk() => {
                let chunk = r.map_err(classify_error)?;
                if chunk.is_none() {
                    // Body finished: free the slot even if the caller keeps the response.
                    self._permit = None;
                }
                Ok(chunk)
            }
        }
    }

    /// Reads the whole body, failing with `Corrupt` when it exceeds `limit` bytes.
    pub async fn bytes(mut self, limit: usize) -> Result<Bytes> {
        let mut buf = BytesMut::new();
        while let Some(c) = self.chunk().await? {
            if buf.len() + c.len() > limit {
                return Err(SvnError::new(
                    ErrorCode::Corrupt,
                    format!("response body exceeds the {limit} byte limit"),
                ));
            }
            buf.extend_from_slice(&c);
        }
        Ok(buf.freeze())
    }
}
