//! Transport-neutral HTTP request/response types.

use bytes::Bytes;
use svn_types::Result;
use url::Url;

/// HTTP methods used by the Subversion protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `OPTIONS`.
    Options,
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `MKCOL`.
    Mkcol,
    /// `COPY`.
    Copy,
    /// `PROPFIND`.
    Propfind,
    /// `PROPPATCH`.
    Proppatch,
    /// `REPORT`.
    Report,
    /// `LOCK`.
    Lock,
    /// `UNLOCK`.
    Unlock,
    /// `MERGE`.
    Merge,
}

impl Method {
    /// Method token as sent on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Options => "OPTIONS",
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Mkcol => "MKCOL",
            Method::Copy => "COPY",
            Method::Propfind => "PROPFIND",
            Method::Proppatch => "PROPPATCH",
            Method::Report => "REPORT",
            Method::Lock => "LOCK",
            Method::Unlock => "UNLOCK",
            Method::Merge => "MERGE",
        }
    }
}

/// Ordered, case-insensitive, multi-valued header list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// Empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a header (existing headers of the same name are kept).
    pub fn push(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.push((name.into(), value.into()));
    }

    /// Builds a list from pairs.
    pub fn from_pairs<N: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (N, V)>,
    ) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(n, v)| (n.into(), v.into()))
                .collect(),
        )
    }

    /// First value of `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every value of `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Iterates all `(name, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}

/// Pull-based request body for payloads that should not be buffered whole.
pub trait BodyStream: Send + std::fmt::Debug {
    /// Next chunk, or `None` at the end.
    fn next_chunk(&mut self) -> Result<Option<Bytes>>;
}

/// Request payload.
#[derive(Debug, Default)]
pub enum Body {
    /// No payload.
    #[default]
    Empty,
    /// Fully buffered payload.
    Bytes(Bytes),
    /// Streamed payload (length unknown, sent chunked or with a caller-known length).
    Stream(Box<dyn BodyStream>),
}

/// A request ready to be executed by a transport.
#[derive(Debug)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Absolute URL.
    pub url: Url,
    /// Headers (without `Authorization`, `Host`, `Content-Length`: the transport owns those).
    pub headers: Headers,
    /// Payload.
    pub body: Body,
}

impl HttpRequest {
    /// Creates a request without headers or body.
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: Headers::new(),
            body: Body::Empty,
        }
    }

    /// Adds a header.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push(name, value);
        self
    }

    /// Sets an in-memory body with a `Content-Type`.
    #[must_use]
    pub fn xml_body(mut self, body: Bytes) -> Self {
        self.headers.push("Content-Type", "text/xml");
        self.body = Body::Bytes(body);
        self
    }

    /// The buffered body, if any (tests and snapshots).
    pub fn body_bytes(&self) -> Option<&Bytes> {
        match &self.body {
            Body::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

/// Status line and headers of a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    /// Status code.
    pub status: u16,
    /// Headers.
    pub headers: Headers,
}

impl ResponseHead {
    /// Creates a response head.
    pub fn new(status: u16, headers: Headers) -> Self {
        Self { status, headers }
    }
}

/// Incremental response parser: feed body bytes as they arrive, collect items as they complete.
pub trait ResponseParser {
    /// Item produced by the parser.
    type Item;

    /// Consumes the next chunk and returns items completed by it.
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Self::Item>>;

    /// Signals end of body; fails if the document is incomplete.
    fn finish(&mut self) -> Result<Vec<Self::Item>>;
}

/// Runs a parser over a complete body, in `chunk` byte pieces (all at once when `chunk == 0`).
pub fn parse_chunked<P: ResponseParser>(
    parser: &mut P,
    body: &[u8],
    chunk: usize,
) -> Result<Vec<P::Item>> {
    let mut items = Vec::new();
    if chunk == 0 {
        items.extend(parser.feed(body)?);
    } else {
        for piece in body.chunks(chunk) {
            items.extend(parser.feed(piece)?);
        }
    }
    items.extend(parser.finish()?);
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_are_case_insensitive_and_multi_valued() {
        let h = Headers::from_pairs([("DAV", "a"), ("dav", "b"), ("Other", "c")]);
        assert_eq!(h.get("Dav"), Some("a"));
        assert_eq!(h.get_all("DAV").collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(h.get("missing"), None);
        assert_eq!(h.iter().count(), 3);
    }

    #[test]
    fn xml_body_sets_content_type_and_payload() {
        let url = Url::parse("http://svn.test.local/x").unwrap();
        let req = HttpRequest::new(Method::Report, url).xml_body(Bytes::from_static(b"<a/>"));
        assert_eq!(req.headers.get("content-type"), Some("text/xml"));
        assert_eq!(req.body_bytes().map(Bytes::as_ref), Some(&b"<a/>"[..]));
        assert_eq!(req.method.as_str(), "REPORT");
    }

    struct Counter(Vec<usize>);

    impl ResponseParser for Counter {
        type Item = usize;

        fn feed(&mut self, chunk: &[u8]) -> Result<Vec<usize>> {
            self.0.push(chunk.len());
            Ok(vec![chunk.len()])
        }

        fn finish(&mut self) -> Result<Vec<usize>> {
            Ok(vec![0])
        }
    }

    #[test]
    fn parse_chunked_splits_and_finishes() {
        let mut c = Counter(Vec::new());
        assert_eq!(parse_chunked(&mut c, b"abcde", 2).unwrap(), [2, 2, 1, 0]);
        let mut c = Counter(Vec::new());
        assert_eq!(parse_chunked(&mut c, b"abcde", 0).unwrap(), [5, 0]);
    }
}
