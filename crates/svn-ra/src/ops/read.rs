//! Remote operation: read. Youngest revision, stat/check-path, directory listings and file
//! contents.
//!
//! Every read addresses a revision root (`!svn/rvr/REV/path`), so a request without a revision
//! first asks for the youngest one; the answer then describes exactly that revision even if
//! commits land in between. Like libsvn_ra_serf's `get_dir`, listings use `Depth: 1` PROPFIND,
//! which every mod_dav_svn from 1.8 on answers with complete entries.

use std::collections::HashSet;

use md5::{Digest, Md5};
use svn_protocol::propfind::{self, MultiStatusParser, PropfindEntry};
use svn_protocol::xml::Ns;
use svn_protocol::{ResponseParser, errors::codes, get, options_request, parse_chunked};
use svn_ra_api::FileMeta;
use svn_types::{
    CancelToken, Checksum, Depth, DirEntry, ErrorCode, NodeKind, PropChange, RelPath, Result,
    Revnum, SvnError,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::update::{RaWire, buffered_error};

pub(crate) fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Newest revision, read from the `SVN-Youngest-Rev` header of a fresh `OPTIONS` (HTTPv2).
///
/// Asked at the session URL. A session reparented to a location the user may not read (the
/// repository root, under path-based authorization that allows only `/trunk`) gets `403` there;
/// it then asks at the URL the session was opened at, which the server already let the user
/// read. The youngest revision is the same anywhere in the repository.
pub(crate) async fn youngest_rev(wire: &RaWire, cancel: &CancelToken) -> Result<Revnum> {
    match youngest_rev_at(wire, wire.session_url(), cancel).await {
        Err(e) if e.code == ErrorCode::Forbidden && wire.opened_url() != wire.session_url() => {
            youngest_rev_at(wire, wire.opened_url(), cancel).await
        }
        other => other,
    }
}

async fn youngest_rev_at(
    wire: &RaWire,
    url: &svn_types::RepoUrl,
    cancel: &CancelToken,
) -> Result<Revnum> {
    let req = options_request(url)?;
    let resp = wire.send_ok(req, cancel).await?;
    let value = resp
        .headers()
        .get("SVN-Youngest-Rev")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| corrupt("OPTIONS answer without SVN-Youngest-Rev"))?;
    value
        .parse::<u64>()
        .map(Revnum)
        .map_err(|_| corrupt(format!("invalid SVN-Youngest-Rev {value:?}")))
}

/// `rev`, or the youngest revision when none was given.
pub(crate) async fn peg(
    wire: &RaWire,
    rev: Option<Revnum>,
    cancel: &CancelToken,
) -> Result<Revnum> {
    match rev {
        Some(r) => Ok(r),
        None => youngest_rev(wire, cancel).await,
    }
}

/// Whether a failed lookup means "no such node" (as opposed to "no such revision", or any
/// other failure, which must surface).
pub(crate) fn is_absent(e: &SvnError) -> bool {
    e.code == ErrorCode::PathNotFound && e.svn_code != Some(codes::FS_NO_SUCH_REVISION)
}

/// Feeds a streamed response body through `parser`, handing each item to `on_item`. Honors the
/// cancel token between chunks (the transport also aborts a pending read on cancel).
pub(crate) async fn stream_items<P: ResponseParser>(
    mut resp: crate::transport::HttpResponse,
    parser: &mut P,
    cancel: &CancelToken,
    mut on_item: impl FnMut(P::Item) -> Result<()>,
) -> Result<()> {
    while let Some(chunk) = resp.chunk().await? {
        for item in parser.feed(&chunk)? {
            on_item(item)?;
        }
        cancel.check()?;
    }
    for item in parser.finish()? {
        on_item(item)?;
    }
    Ok(())
}

/// Entry for `path` at `rev`, `None` when the node does not exist there.
pub(crate) async fn stat(
    wire: &RaWire,
    path: &RelPath,
    rev: Option<Revnum>,
    cancel: &CancelToken,
) -> Result<Option<DirEntry>> {
    let rev = peg(wire, rev, cancel).await?;
    let repo = wire.repo_path(path)?;
    let req = propfind::stat_request(wire.urls(), rev, &repo)?;
    let resp = wire.fetch_small(req, cancel).await?;
    if !resp.status.is_success() {
        let e = buffered_error(&resp);
        return if is_absent(&e) { Ok(None) } else { Err(e) };
    }
    let body = resp.body;
    let entries = parse_chunked(&mut MultiStatusParser::new(), &body, 0)?;
    let first = entries
        .first()
        .ok_or_else(|| corrupt("PROPFIND answered without a response"))?;
    let (node, entry) = propfind::dir_entry_from(wire.urls(), first)?;
    if node != repo {
        return Err(corrupt(format!("PROPFIND for {repo} answered for {node}")));
    }
    Ok(Some(entry))
}

/// Caps for directory listings. A listing that would go beyond either cap fails with
/// [`ErrorCode::LimitExceeded`] instead of returning a partial result.
///
/// The entry cap applies to every depth and is enforced while an answer streams in, so a
/// single huge directory is never buffered beyond it. The level cap only matters for a
/// recursive (`Depth::Infinity`) listing, which costs one request per directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListLimits {
    /// Most entries a listing may hold (for `Files`, the directory's children of any kind).
    pub max_entries: usize,
    /// Deepest level below the listed directory that may be visited (1 = its children).
    pub max_depth: usize,
}

impl Default for ListLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_depth: 512,
        }
    }
}

fn too_many_entries(max: usize) -> SvnError {
    SvnError::new(
        ErrorCode::LimitExceeded,
        format!(
            "the folder listing has more than {max} entries, which is more than OpenSVN \
             lists at once; list a smaller folder or a lower depth"
        ),
    )
}

fn too_deep(max: usize) -> SvnError {
    SvnError::new(
        ErrorCode::LimitExceeded,
        format!(
            "the folder tree is more than {max} levels deep, which is more than OpenSVN \
             lists at once; list a smaller folder or a lower depth"
        ),
    )
}

/// One `Depth: 1` listing: the directory's own entry and its children with their repository
/// paths. Children the protocol layer refuses (other origin, unsafe names) are skipped with a
/// warning rather than failing the whole listing.
///
/// At most `budget` children are accepted: one more fails with [`ErrorCode::LimitExceeded`]
/// while the answer is still streaming (`max` is the overall cap, for the message).
async fn list_one(
    wire: &RaWire,
    rev: Revnum,
    dir: &RelPath,
    budget: usize,
    max: usize,
    cancel: &CancelToken,
) -> Result<(DirEntry, Vec<(RelPath, DirEntry)>)> {
    let req = propfind::list_request(wire.urls(), rev, dir)?;
    let resp = wire.send_ok(req, cancel).await?;
    let mut own = None;
    let mut children = Vec::new();
    // A repeated entry would make a recursive listing walk its subtree again at every level.
    let mut seen = HashSet::new();
    let mut parser = MultiStatusParser::new();
    stream_items(resp, &mut parser, cancel, |e: PropfindEntry| {
        match propfind::dir_entry_from(wire.urls(), &e) {
            Ok((p, entry)) if &p == dir => own = Some(entry),
            Ok((p, entry)) if p.parent().as_ref() == Some(dir) => {
                if seen.insert(p.clone()) {
                    if children.len() >= budget {
                        return Err(too_many_entries(max));
                    }
                    children.push((p, entry));
                } else {
                    tracing::warn!("skipping a repeated listing entry");
                }
            }
            Ok((p, _)) => {
                return Err(corrupt(format!(
                    "listing of {dir} contains unrelated path {p}"
                )));
            }
            Err(err) => {
                tracing::warn!(code = ?err.code, "skipping a listing entry: {}", err.message);
            }
        }
        Ok(())
    })
    .await?;
    let own = own.ok_or_else(|| corrupt("listing does not include the directory itself"))?;
    Ok((own, children))
}

/// Lists the directory `path` at `rev`. `Files` keeps only files, `Immediates` all children,
/// `Infinity` the whole subtree (names below the first level are paths relative to `path`).
///
/// Every listing is bounded by the wire's [`ListLimits`] and fails with
/// [`ErrorCode::LimitExceeded`] when it would exceed them. Repeated entries in an answer are
/// listed once.
pub(crate) async fn list(
    wire: &RaWire,
    path: &RelPath,
    rev: Option<Revnum>,
    depth: Depth,
    cancel: &CancelToken,
) -> Result<Vec<DirEntry>> {
    if matches!(depth, Depth::Exclude | Depth::Unknown) {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "list needs a concrete depth",
        ));
    }
    let rev = peg(wire, rev, cancel).await?;
    let repo = wire.repo_path(path)?;
    let limits = wire.list_limits();
    let max = limits.max_entries;
    let (own, children) = list_one(wire, rev, &repo, max, max, cancel).await?;
    if own.kind != NodeKind::Dir {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("{path} is not a directory"),
        ));
    }
    let mut out = Vec::new();
    match depth {
        Depth::Files => out.extend(
            children
                .into_iter()
                .filter(|(_, e)| e.kind == NodeKind::File)
                .map(|(_, e)| e),
        ),
        Depth::Immediates => out.extend(children.into_iter().map(|(_, e)| e)),
        Depth::Infinity => {
            // Depth-first, one PROPFIND per directory, like `svn ls -R` against servers
            // without `list-report`.
            let mut stack: Vec<(RelPath, DirEntry, usize)> =
                children.into_iter().rev().map(|(p, e)| (p, e, 1)).collect();
            while let Some((p, mut e, level)) = stack.pop() {
                cancel.check()?;
                if level > limits.max_depth {
                    return Err(too_deep(limits.max_depth));
                }
                e.name = relative_name(&repo, &p)?;
                let is_dir = e.kind == NodeKind::Dir;
                out.push(e);
                if is_dir {
                    // Everything already accepted (listed or still on the stack) counts.
                    let budget = max.saturating_sub(out.len() + stack.len());
                    let (_, below) = list_one(wire, rev, &p, budget, max, cancel).await?;
                    stack.extend(below.into_iter().rev().map(|(c, e)| (c, e, level + 1)));
                }
            }
        }
        Depth::Empty | Depth::Exclude | Depth::Unknown => {}
    }
    Ok(out)
}

/// `p` relative to `base` (`base` is a proper ancestor of `p`).
fn relative_name(base: &RelPath, p: &RelPath) -> Result<String> {
    let rest = if base.is_root() {
        Some(p.as_str())
    } else {
        p.as_str()
            .strip_prefix(base.as_str())
            .and_then(|r| r.strip_prefix('/'))
    };
    rest.map(str::to_owned)
        .ok_or_else(|| corrupt(format!("{p} is not below {base}")))
}

/// Parses a 32-digit hex MD5 (`md5-checksum` live property).
fn parse_md5(text: &str) -> Option<[u8; 16]> {
    let t = text.trim();
    if t.len() != 32 {
        return None;
    }
    let mut out = [0_u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(t.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Streams the content of the file `path` at `rev` into `sink` and returns its versioned
/// properties and the MD5 of the streamed bytes.
///
/// The content is checked against the MD5 the server reports for the node (mod_dav_svn always
/// reports one for files); a mismatch is `Corrupt`. If the server reports none, the MD5 is
/// still computed and returned with `verified: false`.
///
/// On any error the sink may hold partial, or complete but unverified, content: the caller
/// must discard it.
pub(crate) async fn get_file(
    wire: &RaWire,
    path: &RelPath,
    rev: Option<Revnum>,
    sink: &mut (dyn AsyncWrite + Send + Unpin),
    cancel: &CancelToken,
) -> Result<FileMeta> {
    let rev = peg(wire, rev, cancel).await?;
    let repo = wire.repo_path(path)?;

    // Properties, node kind and checksum first: a GET on a directory would return an HTML
    // index page, which must never end up in a file.
    let req = propfind::allprop_request(wire.urls(), rev, &repo)?;
    let body = wire.fetch_small_ok(req, cancel).await?;
    let entries = parse_chunked(&mut MultiStatusParser::new(), &body, 0)?;
    let entry = entries
        .first()
        .ok_or_else(|| corrupt("PROPFIND answered without a response"))?;
    let (node, dirent) = propfind::dir_entry_from(wire.urls(), entry)?;
    if node != repo {
        return Err(corrupt(format!("PROPFIND for {repo} answered for {node}")));
    }
    if dirent.kind != NodeKind::File {
        return Err(SvnError::new(
            ErrorCode::PathNotFound,
            format!("{path} is not a file"),
        ));
    }
    let expected = entry
        .get(&Ns::SvnDav, "md5-checksum")
        .and_then(|v| parse_md5(v.text()));
    let props = entry
        .versioned_props()
        .into_iter()
        .map(|(name, value)| PropChange {
            name,
            value: Some(value.into()),
        })
        .collect();

    let req = get::get_request(wire.urls(), rev, &repo)?;
    let mut resp = wire.send_ok(req, cancel).await?;
    let mut hasher = Md5::new();
    while let Some(chunk) = resp.chunk().await? {
        hasher.update(&chunk);
        sink.write_all(&chunk)
            .await
            .map_err(|e| SvnError::from_io("cannot write file content", &e))?;
        cancel.check()?;
    }
    sink.flush()
        .await
        .map_err(|e| SvnError::from_io("cannot write file content", &e))?;
    let digest: [u8; 16] = hasher.finalize().into();
    if let Some(expected) = expected
        && digest != expected
    {
        return Err(corrupt(format!(
            "content of {path} at {rev} does not match the server's checksum"
        )));
    }
    if expected.is_none() {
        tracing::debug!("server reported no checksum for a file; its content is unverified");
    }
    Ok(FileMeta {
        rev,
        props,
        md5: Checksum::Md5(digest),
        verified: expected.is_some(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn md5_parsing() {
        assert_eq!(
            parse_md5("3e079718d4006e9d757934ce54439418").unwrap()[0],
            0x3e
        );
        assert!(parse_md5("3e07").is_none());
        assert!(parse_md5("zz079718d4006e9d757934ce54439418").is_none());
    }

    #[test]
    fn relative_names() {
        let p = |s: &str| RelPath::new(s).unwrap();
        assert_eq!(relative_name(&p("a"), &p("a/b/c")).unwrap(), "b/c");
        assert_eq!(relative_name(&RelPath::root(), &p("a/b")).unwrap(), "a/b");
        assert!(relative_name(&p("a"), &p("ab/c")).is_err());
    }

    #[test]
    fn absent_only_for_missing_nodes() {
        let missing = SvnError::new(ErrorCode::PathNotFound, "x");
        assert!(is_absent(&missing));
        let no_rev =
            SvnError::new(ErrorCode::PathNotFound, "x").with_svn_code(codes::FS_NO_SUCH_REVISION);
        assert!(!is_absent(&no_rev));
        assert!(!is_absent(&SvnError::new(ErrorCode::Forbidden, "x")));
    }
}
