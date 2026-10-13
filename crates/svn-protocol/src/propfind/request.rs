//! `PROPFIND` request builders.

use svn_types::{RelPath, Result, Revnum};

use crate::caps::UrlBuilder;
use crate::http::{HttpRequest, Method};
use crate::xml::XmlWriter;
use crate::xml::ns::{DAV_URI, SVN_DAV_URI};

/// Properties libsvn_ra_serf asks for when it needs a `DirEntry`.
fn entry_body() -> Result<bytes::Bytes> {
    let mut w = XmlWriter::new();
    w.declaration()
        .start("propfind", &[("xmlns", DAV_URI)])
        .start("prop", &[]);
    for local in ["resourcetype", "getcontentlength"] {
        w.empty(local, &[("xmlns", DAV_URI)]);
    }
    w.empty("deadprop-count", &[("xmlns", SVN_DAV_URI)]);
    for local in ["version-name", "creationdate", "creator-displayname"] {
        w.empty(local, &[("xmlns", DAV_URI)]);
    }
    w.end().end();
    w.finish()
}

fn propfind(
    urls: &UrlBuilder,
    rev: Revnum,
    path: &RelPath,
    depth: &str,
    body: bytes::Bytes,
) -> Result<HttpRequest> {
    // The reference client sends no `DAV:` capability headers on PROPFIND.
    Ok(
        HttpRequest::new(Method::Propfind, urls.rev_root(rev, path)?)
            .header("Depth", depth)
            .xml_body(body),
    )
}

/// Stat of one node at peg revision `rev` (`Depth: 0`).
pub fn stat_request(urls: &UrlBuilder, rev: Revnum, path: &RelPath) -> Result<HttpRequest> {
    propfind(urls, rev, path, "0", entry_body()?)
}

/// Listing of a directory at peg revision `rev` (`Depth: 1`).
pub fn list_request(urls: &UrlBuilder, rev: Revnum, path: &RelPath) -> Result<HttpRequest> {
    propfind(urls, rev, path, "1", entry_body()?)
}

/// All properties of one node (`Depth: 0`, `D:allprop`): versioned, custom and DAV-level.
pub fn allprop_request(urls: &UrlBuilder, rev: Revnum, path: &RelPath) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.declaration()
        .start("propfind", &[("xmlns", DAV_URI)])
        .empty("allprop", &[("xmlns", DAV_URI)])
        .end();
    propfind(urls, rev, path, "0", w.finish()?)
}
