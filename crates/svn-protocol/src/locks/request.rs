//! `LOCK`, `UNLOCK` and get-lock request builders. All three address the public URL.

use svn_types::{RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method};
use crate::xml::XmlWriter;
use crate::xml::ns::DAV_URI;

/// Optional parts of a `LOCK` request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockOptions {
    /// Lock comment (sent as `D:owner`).
    pub comment: Option<String>,
    /// Steal an existing lock (`X-SVN-Options: lock-steal`).
    pub steal: bool,
    /// Revision of the working copy file; the server refuses the lock if the file changed since
    /// (`X-SVN-Version-Name`).
    pub current_rev: Option<Revnum>,
    /// Requested lifetime in seconds (`Timeout: Second-N`); Subversion normally sends none.
    pub timeout_secs: Option<u32>,
}

/// Builds `LOCK` for `path`.
pub fn lock_request(urls: &UrlBuilder, path: &RelPath, opts: &LockOptions) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.declaration().start("lockinfo", &[("xmlns", DAV_URI)]);
    w.start("lockscope", &[]).empty("exclusive", &[]).end();
    w.start("locktype", &[]).empty("write", &[]).end();
    if let Some(comment) = &opts.comment {
        w.element("owner", &[], comment);
    }
    w.end();
    let mut req = HttpRequest::new(Method::Lock, urls.public(path)?).xml_body(w.finish()?);
    if opts.steal {
        req = req.header("X-SVN-Options", "lock-steal");
    }
    if let Some(rev) = opts.current_rev {
        req = req.header("X-SVN-Version-Name", rev.0.to_string());
    }
    if let Some(secs) = opts.timeout_secs {
        req = req.header("Timeout", format!("Second-{secs}"));
    }
    Ok(advertise_caps(req))
}

/// Accepts `opaquelocktoken:...` with or without angle brackets and returns it bracketed. The
/// same validation as for tokens read from the server.
pub(crate) fn bracketed_token(token: &str) -> Result<String> {
    let inner = token.trim().trim_start_matches('<').trim_end_matches('>');
    Ok(format!("<{}>", crate::util::validate_lock_token(inner)?))
}

/// Builds `UNLOCK` for `path` with the lock `token`; `break_lock` adds `X-SVN-Options: lock-break`.
pub fn unlock_request(
    urls: &UrlBuilder,
    path: &RelPath,
    token: &str,
    break_lock: bool,
) -> Result<HttpRequest> {
    let mut req = HttpRequest::new(Method::Unlock, urls.public(path)?)
        .header("Lock-Token", bracketed_token(token)?);
    if break_lock {
        req = req.header("X-SVN-Options", "lock-break");
    }
    Ok(advertise_caps(req))
}

/// Builds the `PROPFIND` that reads the lock of `path` (`D:lockdiscovery`, `Depth: 0`).
pub fn get_lock_request(urls: &UrlBuilder, path: &RelPath) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.declaration()
        .start("propfind", &[("xmlns", DAV_URI)])
        .start("prop", &[])
        .empty("lockdiscovery", &[])
        .end()
        .end();
    Ok(HttpRequest::new(Method::Propfind, urls.public(path)?)
        .header("Depth", "0")
        .xml_body(w.finish()?))
}
