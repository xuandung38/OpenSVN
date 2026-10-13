//! Commit request builders: transaction creation, edits, MERGE and abort.
//!
//! Every edit addresses the transaction root announced by the server (`!svn/txr/<txn>/<path>`);
//! the transaction itself is created by `POST` on the `me` resource.

use bytes::Bytes;
use svn_types::{ErrorCode, PropChange, RelPath, Result, Revnum, SvnError};

use super::skel::{create_txn_body, create_txn_with_props_body};
use crate::caps::{Capabilities, UrlBuilder, advertise_caps};
use crate::http::{Body, HttpRequest, Method};
use crate::locks::bracketed_token;
use crate::xml::XmlWriter;
use crate::xml::ns::{CUSTOM_PROPS_URI, DAV_URI, SVN_DAV_URI, SVN_PROPS_URI};

/// Lock a path is held under: the token is presented in an `If:` header.
#[derive(Debug, Clone, Copy)]
pub struct LockToken<'a>(pub &'a str);

fn with_lock(
    urls: &UrlBuilder,
    path: &RelPath,
    lock: Option<LockToken<'_>>,
    req: HttpRequest,
) -> Result<HttpRequest> {
    match lock {
        None => Ok(req),
        Some(LockToken(token)) => Ok(req.header(
            "If",
            format!("<{}> ({})", urls.public(path)?, bracketed_token(token)?),
        )),
    }
}

fn base_header(req: HttpRequest, base_rev: Option<Revnum>) -> HttpRequest {
    match base_rev {
        Some(r) => req.header("X-SVN-Version-Name", r.0.to_string()),
        None => req,
    }
}

/// Builds `POST !svn/me`. Revision properties (normally `svn:log`) travel with the request when
/// the server lists `create-txn-with-props`; otherwise set them afterwards with
/// [`txn_proppatch_request`].
pub fn create_txn_request(
    urls: &UrlBuilder,
    caps: &Capabilities,
    revprops: &[(&str, &[u8])],
) -> Result<HttpRequest> {
    let body = if revprops.is_empty() {
        create_txn_body()
    } else if caps.supports_post("create-txn-with-props") {
        create_txn_with_props_body(revprops)
    } else {
        return Err(SvnError::new(
            ErrorCode::ServerUnsupported,
            "server cannot create a transaction with revision properties",
        ));
    };
    let mut req = HttpRequest::new(Method::Post, urls.me()?)
        .header("Content-Type", "application/vnd.svn-skel");
    req.body = Body::Bytes(body);
    Ok(advertise_caps(req))
}

/// Builds `MKCOL` for a new directory.
pub fn mkcol_request(urls: &UrlBuilder, txn: &str, path: &RelPath) -> Result<HttpRequest> {
    Ok(advertise_caps(HttpRequest::new(
        Method::Mkcol,
        urls.txn_root(txn, path)?,
    )))
}

/// Optional parts of a `PUT`.
#[derive(Debug, Clone, Default)]
pub struct PutOptions<'a> {
    /// Revision of the base text (`X-SVN-Version-Name`); the server refuses stale bases.
    pub base_rev: Option<Revnum>,
    /// MD5 of the base text, lower-case hex (`X-SVN-Base-Fulltext-MD5`).
    pub base_md5: Option<&'a str>,
    /// MD5 of the new text; the server verifies it when it advertises `put-result-checksum`.
    pub result_md5: Option<&'a str>,
    /// Lock held on the path.
    pub lock: Option<LockToken<'a>>,
}

/// Builds `PUT` of a svndiff body (an empty body creates an empty file).
pub fn put_request(
    urls: &UrlBuilder,
    txn: &str,
    path: &RelPath,
    svndiff: Bytes,
    opts: &PutOptions<'_>,
) -> Result<HttpRequest> {
    let mut req = HttpRequest::new(Method::Put, urls.txn_root(txn, path)?)
        .header("Content-Type", "application/vnd.svn-svndiff");
    req.body = Body::Bytes(svndiff);
    req = base_header(req, opts.base_rev);
    for (name, value) in [
        ("X-SVN-Base-Fulltext-MD5", opts.base_md5),
        ("X-SVN-Result-Fulltext-MD5", opts.result_md5),
    ] {
        if let Some(md5) = value {
            check_md5(md5)?;
            req = req.header(name, md5);
        }
    }
    with_lock(urls, path, opts.lock, advertise_caps(req))
}

fn check_md5(md5: &str) -> Result<()> {
    if md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(SvnError::new(
            ErrorCode::Internal,
            "MD5 must be 32 hex digits",
        ))
    }
}

/// Builds `DELETE` of a node inside the transaction.
pub fn delete_request(
    urls: &UrlBuilder,
    txn: &str,
    path: &RelPath,
    base_rev: Option<Revnum>,
    lock: Option<LockToken<'_>>,
) -> Result<HttpRequest> {
    let req = HttpRequest::new(Method::Delete, urls.txn_root(txn, path)?);
    with_lock(urls, path, lock, advertise_caps(base_header(req, base_rev)))
}

/// Builds `COPY` of `src` at `src_rev` to `dst` inside the transaction.
pub fn copy_request(
    urls: &UrlBuilder,
    src_rev: Revnum,
    src: &RelPath,
    txn: &str,
    dst: &RelPath,
) -> Result<HttpRequest> {
    let req = HttpRequest::new(Method::Copy, urls.rev_root(src_rev, src)?)
        .header("Destination", urls.txn_root(txn, dst)?.as_str())
        .header("Overwrite", "F");
    Ok(advertise_caps(req))
}

/// Maps a Subversion property name to its DAV element (`svn:log` -> `S:log`, others -> `C:name`).
fn prop_element(name: &str) -> String {
    match name.strip_prefix("svn:") {
        Some(rest) => format!("S:{rest}"),
        None => format!("C:{name}"),
    }
}

/// Whether `value` can travel as XML character data (valid UTF-8 without forbidden controls).
fn xml_safe(value: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(value).ok()?;
    text.chars()
        .all(|c| matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'))
        .then_some(text)
}

fn propertyupdate_body(changes: &[PropChange]) -> Result<Bytes> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    let mut w = XmlWriter::new();
    w.declaration().start(
        "D:propertyupdate",
        &[
            ("xmlns:D", DAV_URI),
            ("xmlns:V", SVN_DAV_URI),
            ("xmlns:C", CUSTOM_PROPS_URI),
            ("xmlns:S", SVN_PROPS_URI),
        ],
    );
    let sets: Vec<_> = changes.iter().filter(|c| c.value.is_some()).collect();
    if !sets.is_empty() {
        w.start("D:set", &[]).start("D:prop", &[]);
        for c in sets {
            let element = prop_element(&c.name);
            let value = c.value.as_deref().unwrap_or_default();
            match xml_safe(value) {
                Some(text) => w.element(&element, &[], text),
                None => w.element(
                    &element,
                    &[("V:encoding", "base64")],
                    &STANDARD.encode(value),
                ),
            };
        }
        w.end().end();
    }
    let removes: Vec<_> = changes.iter().filter(|c| c.value.is_none()).collect();
    if !removes.is_empty() {
        w.start("D:remove", &[]).start("D:prop", &[]);
        for c in removes {
            w.empty(&prop_element(&c.name), &[]);
        }
        w.end().end();
    }
    w.end();
    w.finish()
}

/// Builds `PROPPATCH` for a node inside the transaction.
pub fn proppatch_request(
    urls: &UrlBuilder,
    txn: &str,
    path: &RelPath,
    changes: &[PropChange],
    base_rev: Option<Revnum>,
    lock: Option<LockToken<'_>>,
) -> Result<HttpRequest> {
    let req = HttpRequest::new(Method::Proppatch, urls.txn_root(txn, path)?)
        .xml_body(propertyupdate_body(changes)?);
    with_lock(urls, path, lock, advertise_caps(base_header(req, base_rev)))
}

/// Builds `PROPPATCH` on the transaction resource itself (revision properties such as `svn:log`).
pub fn txn_proppatch_request(
    urls: &UrlBuilder,
    txn: &str,
    changes: &[PropChange],
) -> Result<HttpRequest> {
    let req =
        HttpRequest::new(Method::Proppatch, urls.txn(txn)?).xml_body(propertyupdate_body(changes)?);
    Ok(advertise_caps(req))
}

/// What to do with locks at commit time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockHandling {
    /// Release the locks the commit used (`release-locks`).
    Release,
    /// Keep them (`keep-locks`).
    Keep,
}

/// Parameters of the final `MERGE`.
#[derive(Debug, Clone)]
pub struct MergeOptions<'a> {
    /// Lock handling.
    pub locks: LockHandling,
    /// Ask the server to omit the per-resource answer (`no-merge-response`).
    pub no_merge_response: bool,
    /// Locks used by the commit as `(path relative to the merge root, token)`.
    pub lock_tokens: &'a [(RelPath, String)],
}

/// Builds the `MERGE` that turns the transaction into a revision. It is addressed to the public
/// URL of `merge_root` (the commit's base directory) and names the transaction in its body.
pub fn merge_request(
    urls: &UrlBuilder,
    merge_root: &RelPath,
    txn: &str,
    opts: &MergeOptions<'_>,
) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.declaration().start("D:merge", &[("xmlns:D", DAV_URI)]);
    w.start("D:source", &[])
        .element("D:href", &[], urls.txn(txn)?.path())
        .end();
    w.empty("D:no-auto-merge", &[]).empty("D:no-checkout", &[]);
    w.start("D:prop", &[]);
    for name in [
        "D:checked-in",
        "D:version-name",
        "D:resourcetype",
        "D:creationdate",
        "D:creator-displayname",
    ] {
        w.empty(name, &[]);
    }
    w.end();
    if !opts.lock_tokens.is_empty() {
        w.start("S:lock-token-list", &[("xmlns:S", "svn:")]);
        for (path, token) in opts.lock_tokens {
            w.start("S:lock", &[])
                .element("lock-path", &[], path.as_str())
                .element("lock-token", &[], token)
                .end();
        }
        w.end();
    }
    w.end();
    let mut options = vec![match opts.locks {
        LockHandling::Release => "release-locks",
        LockHandling::Keep => "keep-locks",
    }];
    if opts.no_merge_response {
        options.push("no-merge-response");
    }
    let req = HttpRequest::new(Method::Merge, urls.public(merge_root)?)
        .xml_body(w.finish()?)
        .header("X-SVN-Options", options.join(" "));
    Ok(advertise_caps(req))
}

/// Builds the `DELETE` that discards a transaction (after a failure or cancellation).
pub fn abort_txn_request(urls: &UrlBuilder, txn: &str) -> Result<HttpRequest> {
    Ok(advertise_caps(HttpRequest::new(
        Method::Delete,
        urls.txn(txn)?,
    )))
}
