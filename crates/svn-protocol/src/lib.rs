//! Sans-IO Subversion HTTPv2 (WebDAV) protocol layer.
//!
//! Builders return an [`HttpRequest`] value; parsers consume response bytes incrementally and
//! never touch a socket, a clock or the file system. A transport crate executes the requests.
//! Everything that comes from the server (paths, names, hrefs) is validated on the way in.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod caps;
pub mod display_path;
pub mod errors;
pub mod get;
pub mod http;
pub mod locks;
pub mod log;
pub mod propfind;
pub mod xml;

mod util;

pub use caps::{
    AcceptedHref, BulkUpdates, Capabilities, DavCaps, HrefKind, UrlBuilder, options_request,
};
pub use display_path::{DisplayChangedPath, RepoDisplayPath, RepoLock};
pub use errors::{error_from_response, map_errcode, map_http_status};
pub use http::{
    Body, BodyStream, Headers, HttpRequest, Method, ResponseHead, ResponseParser, parse_chunked,
};
pub use util::parse_timestamp;
