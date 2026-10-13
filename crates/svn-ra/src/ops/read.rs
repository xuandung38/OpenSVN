//! Remote operation: read. Youngest revision, stat/check-path, directory listings and file
//! contents.
//!
//! Every read addresses a revision root (`!svn/rvr/REV/path`), so a request without a revision
//! first asks for the youngest one; the answer then describes exactly that revision even if
//! commits land in between. Like libsvn_ra_serf's `get_dir`, listings use `Depth: 1` PROPFIND,
//! which every mod_dav_svn from 1.8 on answers with complete entries.

use md5::{Digest, Md5};
use svn_protocol::propfind::{self, MultiStatusParser, PropfindEntry};
use svn_protocol::xml::Ns;
use svn_protocol::{ResponseParser, errors::codes, get, options_request, parse_chunked};
use svn_ra_api::FileMeta;
use svn_types::{
    CancelToken, Depth, DirEntry, ErrorCode, NodeKind, PropChange, RelPath, Result, Revnum,
    SvnError,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::update::{RaWire, error_of};

pub(crate) fn corrupt(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::Corrupt, msg)
}

/// Newest revision, read from the `SVN-Youngest-Rev` header of a fresh `OPTIONS` (HTTPv2).
pub(crate) async fn youngest_rev(wire: &RaWire, cancel: &CancelToken) -> Result<Revnum> {
    let req = options_request(wire.session_url())?;
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
    let resp = wire.send(req, cancel).await?;
    if !resp.status().is_success() {
        let e = error_of(resp, cancel).await;
        return if is_absent(&e) { Ok(None) } else { Err(e) };
    }
    let body = RaWire::read_small(resp).await?;
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

/// One `Depth: 1` listing: the directory's own entry and its children with their repository
/// paths. Children the protocol layer refuses (other origin, unsafe names) are skipped with a
/// warning rather than failing the whole listing.
async fn list_one(
    wire: &RaWire,
    rev: Revnum,
    dir: &RelPath,
    cancel: &CancelToken,
) -> Result<(DirEntry, Vec<(RelPath, DirEntry)>)> {
    let req = propfind::list_request(wire.urls(), rev, dir)?;
    let resp = wire.send_ok(req, cancel).await?;
    let mut own = None;
    let mut children = Vec::new();
    let mut parser = MultiStatusParser::new();
    stream_items(resp, &mut parser, cancel, |e: PropfindEntry| {
        match propfind::dir_entry_from(wire.urls(), &e) {
            Ok((p, entry)) if &p == dir => own = Some(entry),
            Ok((p, entry)) if p.parent().as_ref() == Some(dir) => children.push((p, entry)),
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
    let (own, children) = list_one(wire, rev, &repo, cancel).await?;
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
            let mut stack: Vec<(RelPath, DirEntry)> = children.into_iter().rev().collect();
            while let Some((p, mut e)) = stack.pop() {
                cancel.check()?;
                e.name = relative_name(&repo, &p)?;
                let is_dir = e.kind == NodeKind::Dir;
                out.push(e);
                if is_dir {
                    let (_, below) = list_one(wire, rev, &p, cancel).await?;
                    stack.extend(below.into_iter().rev());
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
/// properties. The content is checked against the MD5 the server reports for the node.
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
    let resp = wire.send_ok(req, cancel).await?;
    let body = RaWire::read_small(resp).await?;
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
    if let Some(expected) = expected {
        let digest: [u8; 16] = hasher.finalize().into();
        if digest != expected {
            return Err(corrupt(format!(
                "content of {path} at {rev} does not match the server's checksum"
            )));
        }
    }
    Ok(FileMeta { rev, props })
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
