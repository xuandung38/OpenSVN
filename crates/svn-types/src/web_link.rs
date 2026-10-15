//! VisualSVN Server web-interface links (`https://host/!/#Repo/view/head/trunk`).
//!
//! Users copy these from the browser's address bar and paste them where a repository URL is
//! expected. The web UI lives under `<prefix>/!/` and names the repository in the fragment; the
//! repository itself is normally served under `<prefix>/svn/<Repo>`. [`parse_visualsvn_web_link`]
//! only builds that candidate: the caller must confirm it with the server before using it.

use crate::error::{ErrorCode, Result, SvnError};
use crate::ids::{RepoUrl, Revnum};

/// Path segment VisualSVN Server serves repositories under by default.
pub const VISUALSVN_SVN_PREFIX: &str = "svn";

/// Candidate repository location derived from a VisualSVN web-interface link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualSvnWebLink {
    /// Repository URL on the same scheme, host and port as the link (`<prefix>/svn/<Repo>[/path]`).
    pub repo_url: RepoUrl,
    /// Revision the link pointed at (`/view/r1234/...`); `None` for `head` or no revision.
    pub peg_rev: Option<Revnum>,
}

fn bad_link(msg: impl Into<String>) -> SvnError {
    SvnError::new(ErrorCode::UnsafePath, msg)
}

/// Parsed URL when `s` has the web-interface shape: http(s), a path ending in `/!/` (or `/!`)
/// and a fragment.
fn web_ui_url(s: &str) -> Option<url::Url> {
    let url = url::Url::parse(s.trim()).ok()?;
    let marker = url.path().ends_with("/!/") || url.path().ends_with("/!");
    (matches!(url.scheme(), "http" | "https") && marker && url.fragment().is_some()).then_some(url)
}

/// Whether `s` looks like a VisualSVN web-interface link (cheap shape test, no validation).
pub fn is_visualsvn_web_link(s: &str) -> bool {
    web_ui_url(s).is_some()
}

/// Refuses text the URL parser would silently rewrite (control characters, `\`, their percent
/// escapes), the same rule `svn_protocol::check_server_url_text` applies to server text.
fn check_text(s: &str) -> Result<()> {
    if let Some(c) = s.chars().find(|c| c.is_control() || *c == '\\') {
        return Err(bad_link(format!(
            "web link contains a forbidden character {c:?}"
        )));
    }
    let bytes = s.as_bytes();
    for (i, _) in s.match_indices('%') {
        let byte = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        if let Some(b) = byte
            && (b < 0x20 || b == 0x7F || b == b'\\')
        {
            return Err(bad_link(format!(
                "web link contains an encoded control character or backslash (%{b:02X})"
            )));
        }
    }
    Ok(())
}

/// Strict percent-decoding to UTF-8.
fn decode(seg: &str) -> Result<String> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let b = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| bad_link("web link has a bad percent escape"))?;
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad_link("web link is not UTF-8 after decoding"))
}

/// One decoded name from the fragment: never empty, `.`, `..`, or containing a separator or a
/// control character, so it cannot step outside `<prefix>/svn/`.
fn check_segment(seg: &str) -> Result<()> {
    if seg.is_empty() || seg == "." || seg == ".." {
        return Err(bad_link("web link contains an empty or dot segment"));
    }
    if seg.chars().any(|c| c == '/' || c == '\\' || c.is_control()) {
        return Err(bad_link("web link contains an encoded separator"));
    }
    Ok(())
}

/// `head` → `None`, `r1234` → `Some(1234)`.
fn parse_rev(s: &str) -> Result<Option<Revnum>> {
    if s.eq_ignore_ascii_case("head") {
        return Ok(None);
    }
    s.strip_prefix(['r', 'R'])
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .map(|n| Some(Revnum(n)))
        .ok_or_else(|| bad_link(format!("web link has an unknown revision {s:?}")))
}

/// Candidate repository URL for a VisualSVN web-interface link.
///
/// Returns `None` when `s` is not a web-interface link (see [`is_visualsvn_web_link`]), so the
/// caller keeps its usual handling. The candidate keeps the link's scheme, host, port and the
/// prefix before `/!/`, then appends `svn/<Repo>` and the path after `/view/<head|rNNN>/`. Names
/// are percent-decoded and encoded again one segment at a time.
pub fn parse_visualsvn_web_link(s: &str) -> Option<Result<VisualSvnWebLink>> {
    let url = web_ui_url(s)?;
    Some(build(s.trim(), url))
}

fn build(raw: &str, url: url::Url) -> Result<VisualSvnWebLink> {
    check_text(raw)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SvnError::new(
            ErrorCode::Internal,
            "repository URL must not contain credentials",
        ));
    }
    if url.host_str().is_none() || url.query().is_some() {
        return Err(bad_link("web link must have a host and no query"));
    }
    // A `?` inside the fragment starts web-UI state, never part of a name (names encode it).
    let fragment = url.fragment().unwrap_or_default();
    let fragment = fragment.split_once('?').map_or(fragment, |(f, _)| f);
    let segs = fragment
        .split('/')
        .filter(|s| !s.is_empty())
        .map(decode)
        .collect::<Result<Vec<_>>>()?;
    for seg in &segs {
        check_segment(seg)?;
    }
    let (repo, rest) = segs
        .split_first()
        .ok_or_else(|| bad_link("web link does not name a repository"))?;
    let (peg_rev, path): (Option<Revnum>, &[String]) = match rest {
        [view, rev, path @ ..] if view == "view" => (parse_rev(rev)?, path),
        // Other pages (history, blame, settings) or a bare `/view`: the repository root.
        _ => (None, &[]),
    };

    let prefix = url
        .path()
        .trim_end_matches('/')
        .strip_suffix('!')
        .unwrap_or_default()
        .to_owned();
    let mut out = url.clone();
    out.set_fragment(None);
    out.set_path(&prefix);
    {
        let mut p = out
            .path_segments_mut()
            .map_err(|()| bad_link("web link cannot be a base"))?;
        p.pop_if_empty().push(VISUALSVN_SVN_PREFIX).push(repo);
        for seg in path {
            p.push(seg);
        }
    }
    let repo_url = RepoUrl::parse(out.as_str())?;
    // Defence in depth: the candidate never leaves the origin the user typed.
    if repo_url.as_url().origin() != url.origin() {
        return Err(bad_link("web link candidate changed origin"));
    }
    Ok(VisualSvnWebLink { repo_url, peg_rev })
}
