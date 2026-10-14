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
pub use config::{ProxyConfig, SVN_COMPAT_VERSION, TransportConfig, USER_AGENT};
pub use redact::{
    RedactedHeaders, RedactingMakeWriter, RedactingWriter, redact_text, redacted_url,
};
pub use request::{BodyStream, HttpRequest, HttpResponse, RequestBody};
pub use retry::{RetryPolicy, classify_status, is_idempotent};
pub use tls::{MemoryTrustStore, TrustStore, sha256_fingerprint};
