//! `OPTIONS` request and response handling.

use svn_types::{ErrorCode, RepoUrl, Result, Revnum, SvnError};
use url::Url;

use crate::errors::error_from_response;
use crate::http::{HttpRequest, Method, ResponseHead};
use crate::util::{corrupt, parse_revnum};
use crate::xml::{Ns, XmlEvent, XmlReader, XmlWriter};

const DAV_SVN_PREFIX: &str = "http://subversion.tigris.org/xmlns/dav/svn/";

/// Capability tokens the client advertises on its own requests, like libsvn_ra_serf.
const CLIENT_CAPS: [&str; 3] = ["depth", "mergeinfo", "log-revprops"];

/// Adds the `DAV:` capability headers every Subversion request carries.
pub(crate) fn advertise_caps(req: HttpRequest) -> HttpRequest {
    CLIENT_CAPS.iter().fold(req, |r, cap| {
        r.header("DAV", format!("{DAV_SVN_PREFIX}{cap}"))
    })
}

/// Server value of `SVN-Allow-Bulk-Updates`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkUpdates {
    /// Bulk (send-all) updates are allowed.
    On,
    /// Bulk updates are refused; the client must use skelta mode.
    Off,
    /// The server prefers bulk updates.
    Prefer,
}

/// Capability tokens advertised in `DAV:` response headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // a flag set; each field mirrors one protocol token
pub struct DavCaps {
    /// `depth`.
    pub depth: bool,
    /// `mergeinfo`.
    pub mergeinfo: bool,
    /// `log-revprops`.
    pub log_revprops: bool,
    /// `atomic-revprops`.
    pub atomic_revprops: bool,
    /// `partial-replay`.
    pub partial_replay: bool,
    /// `inherited-props`.
    pub inherited_props: bool,
    /// `ephemeral-txnprops`.
    pub ephemeral_txnprops: bool,
    /// `reverse-file-revs`.
    pub reverse_file_revs: bool,
    /// `inline-props`.
    pub inline_props: bool,
    /// `replay-rev-resource`.
    pub replay_rev_resource: bool,
    /// `svndiff1`.
    pub svndiff1: bool,
    /// `svndiff2`.
    pub svndiff2: bool,
    /// `list`.
    pub list: bool,
    /// `put-result-checksum`.
    pub put_result_checksum: bool,
}

impl DavCaps {
    fn set(&mut self, token: &str) {
        match token {
            "depth" => self.depth = true,
            "mergeinfo" => self.mergeinfo = true,
            "log-revprops" => self.log_revprops = true,
            "atomic-revprops" => self.atomic_revprops = true,
            "partial-replay" => self.partial_replay = true,
            "inherited-props" => self.inherited_props = true,
            "ephemeral-txnprops" => self.ephemeral_txnprops = true,
            "reverse-file-revs" => self.reverse_file_revs = true,
            "inline-props" => self.inline_props = true,
            "replay-rev-resource" => self.replay_rev_resource = true,
            "svndiff1" => self.svndiff1 = true,
            "svndiff2" => self.svndiff2 = true,
            "list" => self.list = true,
            "put-result-checksum" => self.put_result_checksum = true,
            _ => {} // unknown tokens from newer servers are ignored
        }
    }
}

/// Everything learned from the `OPTIONS` response of an HTTPv2 server.
///
/// Stub values are server-side URL paths (for example `/svn/repo/!svn/rvr`); they are never
/// hard-coded, see [`UrlBuilder`](super::UrlBuilder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Capability tokens.
    pub dav: DavCaps,
    /// `SVN-Me-Resource` path.
    pub me_resource: String,
    /// `SVN-Repository-Root` path (the public location of the repository).
    pub repository_root: String,
    /// `SVN-Repository-UUID`.
    pub uuid: Option<String>,
    /// `SVN-Youngest-Rev`.
    pub youngest_rev: Option<Revnum>,
    /// `SVN-Rev-Stub` path.
    pub rev_stub: String,
    /// `SVN-Rev-Root-Stub` path.
    pub rev_root_stub: String,
    /// `SVN-Txn-Stub` path.
    pub txn_stub: Option<String>,
    /// `SVN-Txn-Root-Stub` path.
    pub txn_root_stub: Option<String>,
    /// `SVN-VTxn-Stub` path.
    pub vtxn_stub: Option<String>,
    /// `SVN-VTxn-Root-Stub` path.
    pub vtxn_root_stub: Option<String>,
    /// `SVN-Supported-Posts` values (`create-txn` is implied when the header is absent).
    pub supported_posts: Vec<String>,
    /// `SVN-Allow-Bulk-Updates`, when sent.
    pub bulk_updates: Option<BulkUpdates>,
    /// `SVN-Repository-MergeInfo`, when sent.
    pub repository_mergeinfo: Option<bool>,
    /// First `D:activity-collection-set` href from the body (unused by HTTPv2).
    pub activity_collection: Option<String>,
}

/// Builds the `OPTIONS` request for `session_url`.
pub fn options_request(session_url: &RepoUrl) -> Result<HttpRequest> {
    let mut w = XmlWriter::new();
    w.declaration()
        .start("D:options", &[("xmlns:D", "DAV:")])
        .element("D:activity-collection-set", &[], "")
        .end();
    let req = HttpRequest::new(Method::Options, session_url.as_url().clone()).xml_body(w.finish()?);
    Ok(advertise_caps(req))
}

fn single(head: &ResponseHead, name: &str) -> Option<String> {
    head.headers
        .get(name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Reduces a stub header value (path or absolute URL on the session origin) to a normalized
/// absolute path without trailing slash.
fn stub_path(session: &Url, name: &str, value: &str) -> Result<String> {
    let url = session
        .join(value)
        .map_err(|e| corrupt(format!("{name} header is not a URL: {e}")))?;
    if url.origin() != session.origin() {
        return Err(SvnError::new(
            ErrorCode::UnsafePath,
            format!("{name} points to a different origin"),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(corrupt(format!(
            "{name} must not carry a query or fragment"
        )));
    }
    let path = url.path().trim_end_matches('/');
    if path.is_empty() {
        return Err(corrupt(format!("{name} is empty")));
    }
    Ok(path.to_owned())
}

impl Capabilities {
    /// Interprets an `OPTIONS` response. Fails with [`ErrorCode::ServerUnsupported`] when the
    /// server does not speak HTTPv2 (no `SVN-Me-Resource`); older DeltaV servers are out of scope.
    pub fn from_options_response(
        session_url: &RepoUrl,
        head: &ResponseHead,
        body: &[u8],
    ) -> Result<Self> {
        if !(200..300).contains(&head.status) {
            return Err(error_from_response(head.status, &head.headers, body));
        }
        let session = session_url.as_url();
        let path_header = |name: &str| -> Result<Option<String>> {
            single(head, name)
                .map(|v| stub_path(session, name, &v))
                .transpose()
        };
        let me_resource = path_header("SVN-Me-Resource")?.ok_or_else(|| {
            SvnError::new(
                ErrorCode::ServerUnsupported,
                "server does not support Subversion HTTP protocol v2 (no SVN-Me-Resource header); \
                 only mod_dav_svn 1.7 or newer is supported",
            )
        })?;
        let missing = |name: &str| corrupt(format!("HTTPv2 server did not send {name}"));
        let repository_root =
            path_header("SVN-Repository-Root")?.ok_or_else(|| missing("SVN-Repository-Root"))?;
        let rev_root_stub =
            path_header("SVN-Rev-Root-Stub")?.ok_or_else(|| missing("SVN-Rev-Root-Stub"))?;
        let rev_stub = path_header("SVN-Rev-Stub")?.ok_or_else(|| missing("SVN-Rev-Stub"))?;

        let mut dav = DavCaps::default();
        for value in head.headers.get_all("DAV") {
            for token in value.split(',') {
                if let Some(cap) = token.trim().strip_prefix(DAV_SVN_PREFIX) {
                    dav.set(cap);
                }
            }
        }
        let supported_posts: Vec<String> = head
            .headers
            .get_all("SVN-Supported-Posts")
            .flat_map(|v| v.split(','))
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        let bulk_updates = single(head, "SVN-Allow-Bulk-Updates").and_then(|v| {
            match v.to_ascii_lowercase().as_str() {
                "on" => Some(BulkUpdates::On),
                "off" => Some(BulkUpdates::Off),
                "prefer" => Some(BulkUpdates::Prefer),
                _ => None,
            }
        });
        let repository_mergeinfo =
            single(head, "SVN-Repository-MergeInfo").map(|v| v.eq_ignore_ascii_case("yes"));
        let youngest_rev = single(head, "SVN-Youngest-Rev")
            .map(|v| parse_revnum(&v))
            .transpose()?;

        Ok(Self {
            dav,
            me_resource,
            repository_root,
            uuid: single(head, "SVN-Repository-UUID"),
            youngest_rev,
            rev_stub,
            rev_root_stub,
            txn_stub: path_header("SVN-Txn-Stub")?,
            txn_root_stub: path_header("SVN-Txn-Root-Stub")?,
            vtxn_stub: path_header("SVN-VTxn-Stub")?,
            vtxn_root_stub: path_header("SVN-VTxn-Root-Stub")?,
            supported_posts,
            bulk_updates,
            repository_mergeinfo,
            activity_collection: activity_collection_href(body),
        })
    }

    /// Whether `post` (for example `create-txn-with-props`) may be used; `create-txn` is
    /// assumed when the server listed nothing.
    pub fn supports_post(&self, post: &str) -> bool {
        if self.supported_posts.is_empty() {
            return post == "create-txn";
        }
        self.supported_posts.iter().any(|p| p == post)
    }
}

/// Best-effort read of `D:activity-collection-set/D:href`; the body is optional and ignored on error.
fn activity_collection_href(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let mut reader = XmlReader::new();
    let mut events = reader.feed(body).ok()?;
    events.extend(reader.finish().ok()?);
    let mut in_set = false;
    let mut in_href = false;
    let mut href = String::new();
    for ev in events {
        match ev {
            XmlEvent::Start(t) if t.name.is(&Ns::Dav, "activity-collection-set") => in_set = true,
            XmlEvent::Start(t) if in_set && t.name.is(&Ns::Dav, "href") => in_href = true,
            XmlEvent::Text(t) if in_href => href.push_str(&t),
            XmlEvent::End(n) if n.is(&Ns::Dav, "href") && in_href => {
                return Some(href.trim().to_owned()).filter(|h| !h.is_empty());
            }
            _ => {}
        }
    }
    None
}
