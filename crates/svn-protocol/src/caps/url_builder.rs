//! URL construction from server-provided stubs and validation of server-provided hrefs.

use svn_types::{ErrorCode, RelPath, RepoUrl, Result, Revnum, SvnError};
use url::Url;

use super::options::Capabilities;
use crate::util::{corrupt, percent_decode};

fn unsafe_href(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::UnsafePath, msg)
}

/// Name of a transaction and which kind of stub addresses it: the server answers
/// `SVN-VTxn-Name` (virtual transaction, used behind write-through proxies) instead of
/// `SVN-Txn-Name` when the client sent a `SVN-VTxn-Name` request header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnName {
    /// Validated name.
    pub name: String,
    /// Addressed through the `vtxn`/`vtxr` stubs.
    pub virtual_: bool,
}

impl TxnName {
    /// A plain transaction.
    pub fn plain(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            virtual_: false,
        }
    }

    /// A virtual transaction.
    pub fn virtual_txn(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            virtual_: true,
        }
    }
}

impl From<&str> for TxnName {
    fn from(name: &str) -> Self {
        Self::plain(name)
    }
}

impl From<&String> for TxnName {
    fn from(name: &String) -> Self {
        Self::plain(name.as_str())
    }
}

impl From<String> for TxnName {
    fn from(name: String) -> Self {
        Self::plain(name)
    }
}

impl From<&TxnName> for TxnName {
    fn from(t: &TxnName) -> Self {
        t.clone()
    }
}

/// What an accepted href points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HrefKind {
    /// The `me` resource.
    Me,
    /// A revision resource (`rev/REV`).
    Rev {
        /// Revision.
        rev: Revnum,
    },
    /// A node in a revision root (`rvr/REV/path`); `path` is the root for the revision root itself.
    RevRoot {
        /// Revision.
        rev: Revnum,
        /// Repository-relative path.
        path: RelPath,
    },
    /// A transaction resource (`txn/NAME` or `vtxn/NAME`).
    Txn {
        /// Transaction name.
        name: String,
    },
    /// A node in a transaction root (`txr/NAME/path` or `vtxr/NAME/path`).
    TxnRoot {
        /// Transaction name.
        name: String,
        /// Repository-relative path.
        path: RelPath,
    },
    /// A node addressed by its public URL below the repository root.
    Public {
        /// Repository-relative path.
        path: RelPath,
    },
}

/// An href that passed origin and prefix checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedHref {
    /// Absolute URL.
    pub url: Url,
    /// Decomposition of the path.
    pub kind: HrefKind,
}

/// Builds request URLs from the stubs a server announced and vets hrefs it sends back.
#[derive(Debug, Clone)]
pub struct UrlBuilder {
    origin: Url,
    root: String,
    me: String,
    rev: String,
    rev_root: String,
    /// `(virtual, stub)`: the plain and the virtual (`SVN-VTxn-Name`) transaction stubs.
    txn: Vec<(bool, String)>,
    txn_root: Vec<(bool, String)>,
    dec: Decoded,
}

/// The stubs in decoded form: hrefs are classified after one round of percent-decoding, so
/// `%21svn` cannot slip past a guard written for `!svn`.
#[derive(Debug, Clone)]
struct Decoded {
    root: String,
    me: String,
    rev: String,
    rev_root: String,
    txn: Vec<String>,
    txn_root: Vec<String>,
    special_parents: Vec<String>,
}

/// Percent-decodes every segment of an absolute path once; an encoded `/` is refused.
fn decode_path(path: &str) -> Result<String> {
    let mut segs = Vec::new();
    for seg in path.split('/') {
        let d = percent_decode(seg)?;
        if d.contains('/') {
            return Err(unsafe_href("encoded slash in href"));
        }
        segs.push(d);
    }
    Ok(segs.join("/"))
}

/// Parent path of an absolute path (`/a/b/c` -> `/a/b`).
fn parent_of(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(String::new, |(p, _)| p.to_owned())
}

/// Whether `path` equals `prefix` or lies below it.
fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Appends segments to an encoded base path, percent-encoding each segment.
fn join_encoded(origin: &Url, base: &str, segments: &[&str]) -> Result<Url> {
    let mut url = origin.clone();
    url.set_path(base);
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|()| corrupt("URL cannot be a base"))?;
        segs.pop_if_empty();
        segs.extend(segments);
    }
    Ok(url)
}

impl UrlBuilder {
    /// Creates a builder for the session at `session_url` using the announced `caps`.
    pub fn new(session_url: &RepoUrl, caps: &Capabilities) -> Result<Self> {
        let mut origin = session_url.as_url().clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        let norm = |p: &str| -> Result<String> {
            let mut u = origin.clone();
            u.set_path(p);
            Ok(u.path().trim_end_matches('/').to_owned())
        };
        let mut txn: Vec<(bool, String)> = Vec::new();
        let mut txn_root: Vec<(bool, String)> = Vec::new();
        for (virt, s) in [(false, &caps.txn_stub), (true, &caps.vtxn_stub)] {
            if let Some(s) = s {
                txn.push((virt, norm(s)?));
            }
        }
        for (virt, s) in [(false, &caps.txn_root_stub), (true, &caps.vtxn_root_stub)] {
            if let Some(s) = s {
                txn_root.push((virt, norm(s)?));
            }
        }
        let me = norm(&caps.me_resource)?;
        let rev = norm(&caps.rev_stub)?;
        let rev_root = norm(&caps.rev_root_stub)?;
        let root = norm(&caps.repository_root)?;
        // Directories that hold the stubs and live inside the public tree: nothing public may
        // be addressed through them except via a recognised stub.
        let mut special_parents: Vec<String> = Vec::new();
        for p in [&me, &rev, &rev_root]
            .into_iter()
            .chain(txn.iter().map(|(_, p)| p))
            .chain(txn_root.iter().map(|(_, p)| p))
        {
            let parent = parent_of(p);
            if parent != root && under(&parent, &root) && !special_parents.contains(&parent) {
                special_parents.push(parent);
            }
        }
        let dec = Decoded {
            root: decode_path(&root)?,
            me: decode_path(&me)?,
            rev: decode_path(&rev)?,
            rev_root: decode_path(&rev_root)?,
            txn: txn
                .iter()
                .map(|(_, p)| decode_path(p))
                .collect::<Result<_>>()?,
            txn_root: txn_root
                .iter()
                .map(|(_, p)| decode_path(p))
                .collect::<Result<_>>()?,
            special_parents: special_parents
                .iter()
                .map(|p| decode_path(p))
                .collect::<Result<_>>()?,
        };
        Ok(Self {
            origin,
            root,
            me,
            rev,
            rev_root,
            txn,
            txn_root,
            dec,
        })
    }

    /// URL of the `me` resource.
    pub fn me(&self) -> Result<Url> {
        join_encoded(&self.origin, &self.me, &[])
    }

    /// URL of the repository root as the public location.
    pub fn repository_root(&self) -> Result<Url> {
        join_encoded(&self.origin, &self.root, &[])
    }

    /// Public URL of `path` (LOCK, UNLOCK, get-lock and get-locks use it).
    pub fn public(&self, path: &RelPath) -> Result<Url> {
        let segs: Vec<&str> = path.components().collect();
        join_encoded(&self.origin, &self.root, &segs)
    }

    /// URL of the revision resource (`rev/REV`).
    pub fn rev(&self, rev: Revnum) -> Result<Url> {
        join_encoded(&self.origin, &self.rev, &[&rev.0.to_string()])
    }

    /// URL of `path` inside the revision root of `rev` (the peg-revision URL).
    pub fn rev_root(&self, rev: Revnum, path: &RelPath) -> Result<Url> {
        let rev = rev.0.to_string();
        let mut segs: Vec<&str> = vec![&rev];
        segs.extend(path.components());
        join_encoded(&self.origin, &self.rev_root, &segs)
    }

    fn txn_name(name: &str) -> Result<()> {
        if !name.is_empty()
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            Ok(())
        } else {
            Err(unsafe_href(format!("invalid transaction name {name:?}")))
        }
    }

    /// URL of a transaction resource (plain or virtual stub, as the name's kind says).
    pub fn txn(&self, txn: impl Into<TxnName>) -> Result<Url> {
        let txn = txn.into();
        Self::txn_name(&txn.name)?;
        let base = Self::stub_for(&self.txn, txn.virtual_, "transaction")?;
        join_encoded(&self.origin, base, &[&txn.name])
    }

    /// URL of `path` in a transaction root.
    pub fn txn_root(&self, txn: impl Into<TxnName>, path: &RelPath) -> Result<Url> {
        let txn = txn.into();
        Self::txn_name(&txn.name)?;
        let base = Self::stub_for(&self.txn_root, txn.virtual_, "transaction root")?;
        let mut segs: Vec<&str> = vec![&txn.name];
        segs.extend(path.components());
        join_encoded(&self.origin, base, &segs)
    }

    fn stub_for<'a>(stubs: &'a [(bool, String)], virt: bool, what: &str) -> Result<&'a String> {
        stubs
            .iter()
            .find(|(v, _)| *v == virt)
            .map(|(_, p)| p)
            .ok_or_else(|| {
                SvnError::new(
                    ErrorCode::ServerUnsupported,
                    format!(
                        "server did not announce a {}{what} stub",
                        if virt { "virtual " } else { "" }
                    ),
                )
            })
    }

    /// Accepts an href from a response only if it names this session's origin and lies below
    /// a known stub or the repository root. Everything else is [`ErrorCode::UnsafePath`].
    pub fn accept_href(&self, href: &str) -> Result<AcceptedHref> {
        if href.is_empty() || href.chars().any(|c| c.is_control() || c == '\\') {
            return Err(unsafe_href(format!("invalid href {href:?}")));
        }
        let (url, raw_path) = if let Some(rest) = href.strip_prefix('/') {
            if rest.starts_with('/') {
                return Err(unsafe_href("network-path reference in href"));
            }
            let raw = href.split(['?', '#']).next().unwrap_or(href);
            (self.origin.join(href), raw.to_owned())
        } else if let Some((scheme, rest)) = href.split_once("://") {
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                return Err(unsafe_href("href with unsupported scheme"));
            }
            let raw = rest.find('/').map_or("", |i| &rest[i..]);
            let raw = raw.split(['?', '#']).next().unwrap_or("");
            (Url::parse(href), raw.to_owned())
        } else {
            return Err(unsafe_href(format!("relative href not accepted: {href:?}")));
        };
        let url = url.map_err(|e| unsafe_href(format!("unparsable href {href:?}: {e}")))?;
        if url.origin() != self.origin.origin() {
            return Err(unsafe_href(format!(
                "href {href:?} is on a different origin"
            )));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(unsafe_href("href with query or fragment"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(unsafe_href("href with embedded credentials"));
        }
        // The URL parser collapses dot segments; look at the raw text before it did.
        for seg in raw_path.split('/') {
            let decoded = percent_decode(seg)
                .map_err(|_| unsafe_href(format!("bad escape in href {href:?}")))?;
            if decoded == "." || decoded == ".." {
                return Err(unsafe_href(format!("dot segment in href {href:?}")));
            }
        }
        let kind = self.classify(&decode_path(url.path())?)?;
        Ok(AcceptedHref { url, kind })
    }

    fn classify(&self, path: &str) -> Result<HrefKind> {
        let d = &self.dec;
        let path = path.trim_end_matches('/');
        if path == d.me {
            return Ok(HrefKind::Me);
        }
        if let Some(rest) = strip_stub(path, &d.rev_root) {
            let (rev, tail) = split_first(rest)?;
            return Ok(HrefKind::RevRoot {
                rev: parse_rev(rev)?,
                path: rel_from_decoded(tail)?,
            });
        }
        if let Some(rest) = strip_stub(path, &d.rev) {
            let (rev, tail) = split_first(rest)?;
            return if tail.is_empty() {
                Ok(HrefKind::Rev {
                    rev: parse_rev(rev)?,
                })
            } else {
                Err(unsafe_href("unexpected path below a revision resource"))
            };
        }
        for stub in &d.txn_root {
            if let Some(rest) = strip_stub(path, stub) {
                let (name, tail) = split_first(rest)?;
                Self::txn_name(name)?;
                return Ok(HrefKind::TxnRoot {
                    name: name.to_owned(),
                    path: rel_from_decoded(tail)?,
                });
            }
        }
        for stub in &d.txn {
            if let Some(rest) = strip_stub(path, stub) {
                let (name, tail) = split_first(rest)?;
                Self::txn_name(name)?;
                return if tail.is_empty() {
                    Ok(HrefKind::Txn {
                        name: name.to_owned(),
                    })
                } else {
                    Err(unsafe_href("unexpected path below a transaction resource"))
                };
            }
        }
        // Any other resource under the directory that holds the stubs (`bc`, `ver`, `vcc`, ...)
        // is a special resource this client does not use.
        if d.special_parents.iter().any(|p| under(path, p)) {
            return Err(unsafe_href(format!(
                "href below an unknown special resource: {path}"
            )));
        }
        if let Some(rest) = strip_stub(path, &d.root) {
            return Ok(HrefKind::Public {
                path: rel_from_decoded(rest)?,
            });
        }
        Err(unsafe_href(format!("href outside the repository: {path}")))
    }
}

/// `Some(rest)` (without leading `/`) when `path` is strictly below `stub`.
fn strip_stub<'a>(path: &'a str, stub: &str) -> Option<&'a str> {
    if path == stub {
        return Some("");
    }
    path.strip_prefix(stub)?.strip_prefix('/')
}

fn split_first(rest: &str) -> Result<(&str, &str)> {
    let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if head.is_empty() {
        return Err(unsafe_href("missing component in href"));
    }
    Ok((head, tail))
}

fn parse_rev(s: &str) -> Result<Revnum> {
    if s.bytes().all(|b| b.is_ascii_digit()) && !s.is_empty() {
        s.parse::<u64>()
            .map(Revnum)
            .map_err(|_| unsafe_href("revision out of range in href"))
    } else {
        Err(unsafe_href(format!("bad revision {s:?} in href")))
    }
}

/// Validates decoded `a/b/c` with [`RelPath::from_server`]; empty means the root.
fn rel_from_decoded(decoded: &str) -> Result<RelPath> {
    if decoded.is_empty() {
        return Ok(RelPath::root());
    }
    RelPath::from_server(decoded)
}
