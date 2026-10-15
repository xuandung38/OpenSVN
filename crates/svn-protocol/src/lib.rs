//! Sans-IO Subversion HTTPv2 (WebDAV) protocol layer.
//!
//! Builders return an [`HttpRequest`] value; parsers consume response bytes incrementally and
//! never touch a socket, a clock or the file system. A transport crate executes the requests.
//! Everything that comes from the server (paths, names, hrefs) is validated on the way in.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod caps;
pub mod commit;
pub mod display_path;
pub mod errors;
pub mod file_revs;
pub mod get;
pub mod http;
pub mod limits;
pub mod list_report;
pub mod locks;
pub mod log;
pub mod propfind;
pub mod update;
pub mod xml;

mod util;

pub use caps::{
    AcceptedHref, BulkPreference, BulkUpdates, Capabilities, DavCaps, HrefKind, ListStrategy,
    TxnName, UrlBuilder, check_server_url_text, options_request, parent_listing_request,
    parent_path_error, parent_path_listing, parent_path_repos, segments_below_root,
};
pub use display_path::RepoDisplayPath;
pub use errors::{error_from_response, map_errcode, map_http_status};
pub use http::{
    Body, BodyStream, Headers, HttpRequest, Method, ResponseHead, ResponseParser, parse_chunked,
};
pub use limits::ParserLimits;
pub use util::parse_timestamp;
