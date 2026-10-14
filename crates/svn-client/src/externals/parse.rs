//! Parsing `svn:externals` values and resolving their URLs, like svn 1.15
//! (`svn_wc_parse_externals_description3` and `svn_wc__resolve_relative_external_url`).

use std::collections::BTreeSet;

use svn_types::{ErrorCode, RelPath, RepoUrl, Result, SvnError};

/// A revision as a definition names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtRev {
    /// The newest revision when the external is fetched.
    Head,
    /// A fixed revision.
    Number(u64),
}

impl ExtRev {
    /// The fixed revision, `None` for HEAD (what wc.db records as the pinned revision).
    pub fn number(self) -> Option<u64> {
        match self {
            Self::Head => None,
            Self::Number(n) => Some(n),
        }
    }
}

/// One line of an `svn:externals` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalItem {
    /// Where the external goes, relative to the directory carrying the property. Validated as
    /// untrusted repository text: no `..`, no absolute path, no administrative directory name.
    pub target: RelPath,
    /// URL as written (absolute or one of the relative forms `^/`, `../`, `//`, `/`), without
    /// the peg revision.
    pub url: String,
    /// Peg revision (`URL@PEG`; HEAD when absent).
    pub peg: ExtRev,
    /// Operative revision (`-r N`; the peg revision when absent).
    pub revision: ExtRev,
}

fn parse_error(dir: &str, detail: impl std::fmt::Display) -> SvnError {
    SvnError::new(
        ErrorCode::WcInvalidOperation,
        format!("error parsing svn:externals property on '{dir}': {detail}"),
    )
    .with_svn_code(195_005)
}

/// Splits one definition line into words like `apr_tokenize_to_argv`: whitespace separates
/// words, single or double quotes group them and a backslash takes the next character literally.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            q @ ('"' | '\'') if quote.is_none() => {
                quote = Some(q);
                in_word = true;
            }
            q if quote == Some(q) => quote = None,
            c if c.is_whitespace() && quote.is_none() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// `svn_path_is_url`: a scheme (letters, digits, `+ - .`) followed by `://`.
fn is_absolute_url(s: &str) -> bool {
    let Some(pos) = s.find("://") else {
        return false;
    };
    let scheme = &s[..pos];
    !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// A revision word: a number or `HEAD`. Dates are valid svn syntax that this client does not
/// resolve yet.
fn revision_word(dir: &str, word: &str, line: &str, allow_head: bool) -> Result<ExtRev> {
    if let Ok(n) = word.parse::<u64>() {
        return Ok(ExtRev::Number(n));
    }
    if allow_head && word.eq_ignore_ascii_case("HEAD") {
        return Ok(ExtRev::Head);
    }
    if word.starts_with('{') {
        return Err(SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "svn:externals on '{dir}': revision dates ('{word}') are not supported yet; use a \
                 revision number"
            ),
        ));
    }
    Err(parse_error(dir, format!("'{line}'")))
}

/// Splits `URL@PEG` like `svn_opt_parse_path`: the last `@` starts the peg revision; a
/// trailing `@` only escapes an `@` inside the URL.
fn split_peg(dir: &str, token: &str, line: &str) -> Result<(String, ExtRev)> {
    let Some(at) = token.rfind('@') else {
        return Ok((token.to_owned(), ExtRev::Head));
    };
    // An `@` in the scheme/authority part (`user@host`) is not a peg revision.
    let path_start = token
        .find("://")
        .map(|p| p + 3 + token[p + 3..].find('/').unwrap_or(token.len() - p - 3))
        .unwrap_or(0);
    if at < path_start {
        return Ok((token.to_owned(), ExtRev::Head));
    }
    let (url, rest) = (&token[..at], &token[at + 1..]);
    if rest.is_empty() {
        return Ok((url.to_owned(), ExtRev::Head));
    }
    Ok((url.to_owned(), revision_word(dir, rest, line, true)?))
}

/// Normalizes a target the way svn canonicalizes it (`a//b/`, `./a`) and validates it as
/// untrusted repository text.
fn target_relpath(dir: &str, raw: &str) -> Result<RelPath> {
    let invalid = || {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!(
                "invalid svn:externals property on '{dir}': target '{raw}' is an absolute path, \
                 involves '..' or is not a valid path"
            ),
        )
        .with_svn_code(195_005)
    };
    // svn uses the platform style; `\` is a separator on Windows and refused elsewhere.
    let raw_slash = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.to_owned()
    };
    if raw_slash.starts_with('/') || (cfg!(windows) && raw_slash.contains(':')) {
        return Err(invalid());
    }
    let parts: Vec<&str> = raw_slash
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() {
        return Err(invalid());
    }
    RelPath::from_server(&parts.join("/")).map_err(|_| invalid())
}

/// Parses the `svn:externals` value of the directory `dir` (used in messages). A line svn
/// would reject fails the whole value, as in svn; so does a target named twice.
pub fn parse_externals(dir: &str, value: &[u8]) -> Result<Vec<ExternalItem>> {
    let text =
        std::str::from_utf8(value).map_err(|_| parse_error(dir, "the value is not UTF-8"))?;
    let mut out: Vec<ExternalItem> = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = words(line);
        if !(2..=4).contains(&tokens.len()) {
            return Err(parse_error(dir, format!("'{line}'")));
        }
        // `-r N` / `-rN` as the first or second word (svn's find_and_remove_externals_revision).
        let mut rev_idx = None;
        let mut revision = None;
        for i in 0..2.min(tokens.len()) {
            if tokens[i].starts_with("-r") {
                let word = if tokens[i] == "-r" {
                    if i + 1 >= tokens.len() {
                        return Err(parse_error(dir, format!("'{line}'")));
                    }
                    let w = tokens.remove(i + 1);
                    tokens.remove(i);
                    w
                } else {
                    tokens.remove(i)[2..].to_owned()
                };
                // `-r HEAD` is refused by svn: an operative revision is a number or a date.
                revision = Some(revision_word(dir, &word, line, false)?);
                rev_idx = Some(i);
                break;
            }
        }
        if tokens.len() != 2 {
            return Err(parse_error(dir, format!("'{line}'")));
        }
        let (t0, t1) = (&tokens[0], &tokens[1]);
        let (t0_url, t1_url) = (is_absolute_url(t0), is_absolute_url(t1));
        if t0_url && t1_url {
            return Err(parse_error(
                dir,
                format!("cannot use two absolute URLs ('{t0}' and '{t1}') in an external"),
            ));
        }
        if (rev_idx == Some(0) && t1_url) || (rev_idx == Some(1) && t0_url) {
            return Err(parse_error(
                dir,
                "cannot use a URL as the target directory of an external",
            ));
        }
        let item = if rev_idx == Some(0) || (rev_idx.is_none() && (t0_url || !t1_url)) {
            // svn 1.5+: `[-r N] URL[@PEG] PATH`.
            let (url, peg) = split_peg(dir, t0, line)?;
            ExternalItem {
                target: target_relpath(dir, t1)?,
                url,
                peg,
                revision: revision.unwrap_or(peg),
            }
        } else {
            // Pre-1.5: `PATH [-r N] URL`; the URL has no peg and the peg is the revision.
            let rev = revision.unwrap_or(ExtRev::Head);
            ExternalItem {
                target: target_relpath(dir, t0)?,
                url: t1.clone(),
                peg: rev,
                revision: rev,
            }
        };
        let key = if crate::status::CASE_INSENSITIVE_FS {
            item.target.as_str().to_lowercase()
        } else {
            item.target.as_str().to_owned()
        };
        if !seen.insert(key) {
            return Err(SvnError::new(
                ErrorCode::WcInvalidOperation,
                format!(
                    "invalid svn:externals property on '{dir}': target '{}' appears more than \
                     once",
                    item.target
                ),
            )
            .with_svn_code(195_005));
        }
        out.push(item);
    }
    Ok(out)
}

/// Applies `.` and `..` segments of `rel` to `base` (both `/`-separated paths without the
/// leading slash). `None` when `..` would leave the server root.
fn resolve_dots(base: &[&str], rel: &str) -> Option<String> {
    let mut segs: Vec<&str> = base.iter().copied().filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop()?;
            }
            s => segs.push(s),
        }
    }
    Some(segs.join("/"))
}

/// Resolves the URL of a definition on the directory whose URL is `dir_url`, in the
/// repository rooted at `repos_root`:
///
/// - `../x` is relative to `dir_url`; `^/x` to `repos_root` (and may leave it: `^/../other`);
/// - `//host/x` keeps the scheme of `dir_url`; `/x` keeps its scheme, host and port;
/// - anything else must be an absolute URL.
///
/// The result may name another host: callers open it through the same session policy (TLS
/// default-deny, authentication) as any other URL.
pub fn resolve_url(url: &str, dir_url: &RepoUrl, repos_root: &RepoUrl) -> Result<RepoUrl> {
    let bad = |why: &str| {
        SvnError::new(
            ErrorCode::WcInvalidOperation,
            format!("invalid external URL '{url}': {why}"),
        )
        .with_svn_code(195_005)
    };
    let origin = |u: &RepoUrl| {
        let u = u.as_url();
        let host = u.host_str().unwrap_or_default();
        match u.port() {
            Some(p) => format!("{}://{host}:{p}", u.scheme()),
            None => format!("{}://{host}", u.scheme()),
        }
    };
    let segments = |u: &RepoUrl| -> Vec<String> {
        u.as_url()
            .path()
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let resolved = if is_absolute_url(url) {
        url.to_owned()
    } else if let Some(rest) = url.strip_prefix("^/") {
        let base = segments(repos_root);
        let base: Vec<&str> = base.iter().map(String::as_str).collect();
        let path = resolve_dots(&base, rest)
            .ok_or_else(|| bad("'..' goes above the root of the server"))?;
        format!("{}/{path}", origin(repos_root))
    } else if url.starts_with("../") {
        let base = segments(dir_url);
        let base: Vec<&str> = base.iter().map(String::as_str).collect();
        let path = resolve_dots(&base, url)
            .ok_or_else(|| bad("'..' goes above the root of the server"))?;
        format!("{}/{path}", origin(dir_url))
    } else if let Some(rest) = url.strip_prefix("//") {
        if rest.split('/').any(|s| s == "..") {
            return Err(bad("a scheme-relative URL cannot contain '..'"));
        }
        format!("{}://{rest}", dir_url.as_url().scheme())
    } else if url.starts_with('/') {
        if url.split('/').any(|s| s == "..") {
            return Err(bad("a server-relative URL cannot contain '..'"));
        }
        format!("{}{url}", origin(dir_url))
    } else {
        return Err(bad(
            "not an absolute URL and not one of the relative forms ^/, ../, //, /",
        ));
    };
    RepoUrl::parse(&resolved).map_err(|e| {
        SvnError::new(
            e.code,
            format!("invalid external URL '{url}' ({resolved}): {}", e.message),
        )
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn p(v: &str) -> Vec<(String, String, ExtRev, ExtRev)> {
        parse_externals("d", v.as_bytes())
            .expect("parse")
            .into_iter()
            .map(|i| (i.target.as_str().to_owned(), i.url, i.peg, i.revision))
            .collect()
    }

    fn item(t: &str, u: &str, peg: ExtRev, rev: ExtRev) -> (String, String, ExtRev, ExtRev) {
        (t.to_owned(), u.to_owned(), peg, rev)
    }

    use ExtRev::{Head, Number as N};

    #[test]
    fn new_format_with_pegs_and_revisions() {
        assert_eq!(
            p(
                "^/lib ext\n-r 1 ^/lib a1\n^/lib@1 a2\n-r1 ^/lib@3 a3\n^/lib@HEAD a4\nhttp://h/r/x@ y"
            ),
            [
                item("ext", "^/lib", Head, Head),
                item("a1", "^/lib", Head, N(1)),
                item("a2", "^/lib", N(1), N(1)),
                item("a3", "^/lib", N(3), N(1)),
                item("a4", "^/lib", Head, Head),
                item("y", "http://h/r/x", Head, Head),
            ]
        );
    }

    #[test]
    fn old_format_pegs_the_revision() {
        assert_eq!(
            p("a3 -r1 http://h/r/lib\nthird/party -r 21 http://h/r/x\next http://h/r/l@2"),
            [
                item("a3", "http://h/r/lib", N(1), N(1)),
                item("third/party", "http://h/r/x", N(21), N(21)),
                item("ext", "http://h/r/l@2", Head, Head),
            ]
        );
    }

    #[test]
    fn quoting_comments_and_canonical_targets() {
        assert_eq!(
            p("# c\n\n'^/a b' \"sp ace\"\n^/c ./x//y/\r\n^/d with\\ space"),
            [
                item("sp ace", "^/a b", Head, Head),
                item("x/y", "^/c", Head, Head),
                item("with space", "^/d", Head, Head),
            ]
        );
    }

    #[test]
    fn invalid_lines_fail_the_whole_value() {
        for bad in [
            "^/a",
            "^/a b c d e",
            "-r HEAD ^/a b",
            "^/a ../escape",
            "^/a /abs",
            "^/a .svn",
            "^/a x/.SVN/y",
            "http://h/a http://h/b",
            "-r1 ^/a http://h/b",
            "^/a x\n^/b x",
            "^/a@BASE x",
            "^/a@x1 x",
        ] {
            assert!(parse_externals("d", bad.as_bytes()).is_err(), "{bad}");
        }
        let date = parse_externals("d", b"-r {2020-01-01} ^/a x").unwrap_err();
        assert!(date.message.contains("not supported"), "{date}");
    }

    fn u(s: &str) -> RepoUrl {
        RepoUrl::parse(s).unwrap()
    }

    #[test]
    fn relative_urls_resolve_like_svn() {
        let root = u("https://h.example:8443/svn/repo");
        let dir = u("https://h.example:8443/svn/repo/trunk/sub");
        let r = |s: &str| resolve_url(s, &dir, &root).map(|x| x.to_string());
        assert_eq!(r("^/lib").unwrap(), "https://h.example:8443/svn/repo/lib");
        assert_eq!(
            r("^/../other/trunk").unwrap(),
            "https://h.example:8443/svn/other/trunk"
        );
        assert_eq!(
            r("../lib").unwrap(),
            "https://h.example:8443/svn/repo/trunk/lib"
        );
        assert_eq!(r("../../../x").unwrap(), "https://h.example:8443/svn/x");
        assert_eq!(r("/svn/r2/x").unwrap(), "https://h.example:8443/svn/r2/x");
        assert_eq!(
            r("//other.example/svn/x").unwrap(),
            "https://other.example/svn/x"
        );
        assert_eq!(
            r("http://abs.example/r/x").unwrap(),
            "http://abs.example/r/x"
        );
        for bad in [
            "../../../../../x",
            "^/../../../x",
            "/svn/../x",
            "//h/../x",
            "lib",
            "svn://h/r",
            "file:///r",
            "https://user:pw@h/r",
        ] {
            assert!(r(bad).is_err(), "{bad}");
        }
    }
}
