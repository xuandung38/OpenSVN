//! `GET` of file contents at a peg revision. The response is raw bytes and is not parsed here.

use svn_types::{RelPath, Result, Revnum};

use crate::caps::UrlBuilder;
use crate::http::{HttpRequest, Method};

/// Builds `GET rvr/REV/path`. Stream the response body to its destination; do not buffer it.
pub fn get_request(urls: &UrlBuilder, rev: Revnum, path: &RelPath) -> Result<HttpRequest> {
    Ok(HttpRequest::new(Method::Get, urls.rev_root(rev, path)?))
}
