//! HTTP transport: connection pool, TLS policy, retry, cancellation and redacted logging.
//!
//! [`HttpTransport`] is the only component that talks to the network. Protocol crates create an
//! [`HttpRequest`] and read the [`HttpResponse`] body chunk by chunk; nothing here buffers a whole
//! body unless the caller asks for it with [`HttpResponse::bytes`].

mod client;
mod config;
mod redact;
mod request;
mod retry;
mod tls;

pub use client::HttpTransport;
pub use config::{
    DEFAULT_CONNECTIONS, DEFAULT_POOL_IDLE_TIMEOUT, DEFAULT_WRITE_CONNECTION_REQUESTS,
    DEFAULT_WRITE_REUSE_WINDOW, MAX_CONNECTIONS, MIN_CONNECTIONS, ProxyConfig, SVN_COMPAT_VERSION,
    TransportConfig, USER_AGENT,
};
pub use redact::{
    RedactedHeaders, RedactingMakeWriter, RedactingWriter, redact_text, redacted_url,
};
pub use request::{BodyStream, BufferedResponse, HttpRequest, HttpResponse, RequestBody};
pub use retry::{RetryPolicy, classify_status, is_idempotent, needs_new_connection};
pub use tls::{MemoryTrustStore, TrustStore, sha256_fingerprint};
