//! `S:log-report` request.

use svn_types::{RelPath, Result, Revnum};

use crate::caps::{UrlBuilder, advertise_caps};
use crate::http::{HttpRequest, Method};
use crate::xml::XmlWriter;

/// Which revision properties the server should return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogRevprops {
    /// `svn:author`, `svn:date`, `svn:log` (what `svn log` asks for).
    Default,
    /// Every revision property (`S:all-revprops`).
    All,
    /// None (`S:no-revprops`).
    None,
    /// An explicit list.
    Only(Vec<String>),
}

/// Options of a log request (`svn_ra_get_log2` arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    /// First revision of the range (newest when walking backwards).
    pub start: Option<Revnum>,
    /// Last revision of the range.
    pub end: Option<Revnum>,
    /// Maximum number of items.
    pub limit: Option<u32>,
    /// Include changed paths (`svn log -v`).
    pub discover_changed_paths: bool,
    /// Stop at copies (`--stop-on-copy`).
    pub strict_node_history: bool,
    /// Include merged revisions (`-g`).
    pub include_merged_revisions: bool,
    /// Revision properties wanted.
    pub revprops: LogRevprops,
    /// Paths relative to the request URL; empty means the URL's own node.
    pub paths: Vec<RelPath>,
}

impl Default for LogRequest {
    fn default() -> Self {
        Self {
            start: None,
            end: None,
            limit: None,
            discover_changed_paths: false,
            strict_node_history: false,
            include_merged_revisions: false,
            revprops: LogRevprops::Default,
            paths: Vec::new(),
        }
    }
}

/// Builds `REPORT rvr/PEG/node` with an `S:log-report` body.
pub fn log_request(
    urls: &UrlBuilder,
    peg: Revnum,
    node: &RelPath,
    opts: &LogRequest,
) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.start("S:log-report", &[("xmlns:S", "svn:")]);
    if let Some(r) = opts.start {
        w.element("S:start-revision", &[], &r.0.to_string());
    }
    if let Some(r) = opts.end {
        w.element("S:end-revision", &[], &r.0.to_string());
    }
    if let Some(n) = opts.limit {
        w.element("S:limit", &[], &n.to_string());
    }
    for (on, name) in [
        (opts.discover_changed_paths, "S:discover-changed-paths"),
        (opts.strict_node_history, "S:strict-node-history"),
        (opts.include_merged_revisions, "S:include-merged-revisions"),
    ] {
        if on {
            w.empty(name, &[]);
        }
    }
    match &opts.revprops {
        LogRevprops::Default => {
            for p in ["svn:author", "svn:date", "svn:log"] {
                w.element("S:revprop", &[], p);
            }
        }
        LogRevprops::All => {
            w.empty("S:all-revprops", &[]);
        }
        LogRevprops::None => {
            w.empty("S:no-revprops", &[]);
        }
        LogRevprops::Only(names) => {
            for p in names {
                w.element("S:revprop", &[], p);
            }
        }
    }
    if opts.paths.is_empty() {
        w.element("S:path", &[], "");
    }
    for p in &opts.paths {
        w.element("S:path", &[], p.as_str());
    }
    w.empty("S:encode-binary-props", &[]);
    w.end();
    let req = HttpRequest::new(Method::Report, urls.rev_root(peg, node)?).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}
