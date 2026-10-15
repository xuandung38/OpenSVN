//! A URL that is not a repository but the repository list of an Apache `SVNParentPath` location
//! (VisualSVN Server's `https://host/svn/`). mod_dav_svn answers the session's `OPTIONS` there
//! with a bare HTTP 500; a `GET` returns the list (`SVNListParentPath On`), as HTML or, with
//! `SVNIndexXSLT` (VisualSVN), as the XML index. Both carry the title
//! "Collection of Repositories" and one `href="<name>/"` per repository.

use svn_types::{ErrorCode, ParentRepo, RepoUrl, Result, SvnError};

use crate::errors::sanitize_message;
use crate::http::{HttpRequest, Method};
use crate::util::percent_decode;

/// Most repository names quoted in the error.
const MAX_NAMES: usize = 10;
/// Longest repository name quoted (bytes).
const MAX_NAME_BYTES: usize = 120;

/// `GET` of the listing page at `url` (with the trailing slash mod_dav_svn expects).
pub fn parent_listing_request(url: &RepoUrl) -> Result<HttpRequest> {
    let mut u = url.as_url().clone();
    if !u.path().ends_with('/') {
        let path = format!("{}/", u.path());
        u.set_path(&path);
    }
    Ok(HttpRequest::new(Method::Get, u))
}

/// The repository names of a parent-path listing page, as shown to the user, or `None` when
/// `body` is not one.
pub fn parent_path_listing(body: &[u8]) -> Option<Vec<String>> {
    Some(
        listed_names(body)?
            .iter()
            .filter_map(|n| display_name(n))
            .collect(),
    )
}

/// The repositories of the parent-path listing page `body` served at `url`, or `None` when
/// `body` is not one. Each URL is `url` plus the repository name as one percent-encoded path
/// segment (`Dự án` → `D%E1%BB%B1%20%C3%A1n`), whatever spelling the page used.
pub fn parent_path_repos(url: &RepoUrl, body: &[u8]) -> Option<Vec<ParentRepo>> {
    let mut out = Vec::new();
    for raw in listed_names(body)? {
        let Some(name) = display_name(&raw) else {
            continue;
        };
        let mut u = url.as_url().clone();
        {
            let Ok(mut segs) = u.path_segments_mut() else {
                return None;
            };
            segs.pop_if_empty().push(&raw);
        }
        out.push(ParentRepo {
            name,
            url: u.to_string(),
        });
    }
    Some(out)
}

/// The decoded names linked from a listing page, in page order and without duplicates.
fn listed_names(body: &[u8]) -> Option<Vec<String>> {
    let text = String::from_utf8_lossy(body);
    if !text.contains("Collection of Repositories") {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("href=\"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let href = unescape_markup(&rest[..end]);
        rest = &rest[end..];
        // Repository links are relative and end with '/'; skip the stylesheet, the project
        // link and `../`.
        let Some(name) = href.strip_suffix('/') else {
            continue;
        };
        if name.is_empty() || name.contains(['/', ':', '?', '#']) || name.starts_with('.') {
            continue;
        }
        if let Ok(decoded) = percent_decode(name)
            && !decoded.is_empty()
            && !decoded.contains('/')
            && !names.contains(&decoded)
        {
            names.push(decoded);
        }
    }
    Some(names)
}

/// A listed name made safe to show; `None` when nothing printable is left.
fn display_name(raw: &str) -> Option<String> {
    let clean = sanitize_message(&raw.replace(['\n', '\t'], " "), MAX_NAME_BYTES);
    let clean = clean.trim();
    (!clean.is_empty()).then(|| clean.to_owned())
}

/// Resolves the character references mod_dav_svn writes in attribute values (`&amp;` for a
/// name with `&`). An unknown or malformed reference is kept as written.
fn unescape_markup(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let resolved = rest.find(';').and_then(|end| {
            let c = match &rest[1..end] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                r => {
                    let n = match r.strip_prefix("#x").or_else(|| r.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => r.strip_prefix('#')?.parse().ok()?,
                    };
                    char::from_u32(n)?
                }
            };
            Some((c, end))
        });
        match resolved {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The error for opening a parent-path listing as a repository; it carries `repos` (see
/// [`SvnError::parent_repositories`]).
pub fn parent_path_error(url: &RepoUrl, repos: &[ParentRepo]) -> SvnError {
    let base = url.as_url().as_str().trim_end_matches('/');
    let mut message = format!(
        "{base} is the list of repositories on this server, not a repository; add a \
         repository name to the URL"
    );
    if let Some(first) = repos.first() {
        message.push_str(&format!(" (for example {})", first.url));
        let shown: Vec<&str> = repos.iter().take(MAX_NAMES).map(|r| r.name.as_str()).collect();
        message.push_str(&format!(". Repositories here: {}", shown.join(", ")));
        if repos.len() > MAX_NAMES {
            message.push_str(&format!(" and {} more", repos.len() - MAX_NAMES));
        }
    }
    SvnError::new(ErrorCode::PathNotFound, message).with_repositories(repos.to_vec())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const HTML: &str = r#"<html><head><title>Collection of Repositories</title></head>
<body>
 <h2>Collection of Repositories</h2>
 <ul>
  <li><a href="D%e1%bb%b1%20%c3%a1n%20Ti%e1%ba%bfng%20Vi%e1%bb%87t/">Dự án Tiếng Việt/</a></li>
  <li><a href="My%20Project/">My Project/</a></li>
  <li><a href="MyRepo/">MyRepo/</a></li>
 </ul>
</body></html>"#;

    const XML: &str = r#"<?xml version="1.0"?>
<?xml-stylesheet type="text/xsl" href="/svnindex.xsl"?>
<svn version="1.14.3 (r1914484)"
     href="http://subversion.apache.org/">
  <index rev="0" path="Collection of Repositories">
    <dir name="Hooked" href="Hooked/" />
    <dir name="My Project" href="My%20Project/" />
    <dir name="evil" href="ev%1bil/" />
  </index>
</svn>"#;

    #[test]
    fn html_listing_names_are_decoded() {
        assert_eq!(
            parent_path_listing(HTML.as_bytes()).unwrap(),
            ["Dự án Tiếng Việt", "My Project", "MyRepo"]
        );
    }

    #[test]
    fn xml_index_listing_skips_stylesheet_and_project_links() {
        assert_eq!(
            parent_path_listing(XML.as_bytes()).unwrap(),
            ["Hooked", "My Project", "evil"]
        );
    }

    #[test]
    fn other_pages_are_not_listings() {
        assert!(parent_path_listing(b"<html><title>500 Internal Server Error</title>").is_none());
        let repo_index = r#"<svn><index rev="3" path="/" base="MyRepo"><updir href="../"/>
            <dir name="trunk" href="trunk/" /></index></svn>"#;
        assert!(parent_path_listing(repo_index.as_bytes()).is_none());
    }

    fn repos_of(url: &str, body: &str) -> Vec<ParentRepo> {
        parent_path_repos(&RepoUrl::parse(url).unwrap(), body.as_bytes()).unwrap()
    }

    #[test]
    fn error_names_the_repositories() {
        let url = RepoUrl::parse("https://svn.example.com/svn").unwrap();
        let repos: Vec<ParentRepo> = (0..12)
            .map(|i| ParentRepo {
                name: format!("r{i}"),
                url: format!("https://svn.example.com/svn/r{i}"),
            })
            .collect();
        let err = parent_path_error(&url, &repos);
        assert_eq!(err.code, ErrorCode::PathNotFound);
        assert!(
            err.message
                .contains("for example https://svn.example.com/svn/r0"),
            "{}",
            err.message
        );
        assert!(err.message.ends_with("r9 and 2 more"), "{}", err.message);
        assert_eq!(err.parent_repositories().unwrap().len(), 12);
        let none = parent_path_error(&url, &[]);
        assert!(!none.message.contains("for example"), "{}", none.message);
        assert_eq!(none.parent_repositories().map(<[_]>::len), Some(0));
    }

    #[test]
    fn html_listing_gives_encoded_repository_urls() {
        let repos = repos_of("https://svn.example.com/svn/", HTML);
        let got: Vec<(&str, &str)> = repos
            .iter()
            .map(|r| (r.name.as_str(), r.url.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "Dự án Tiếng Việt",
                    "https://svn.example.com/svn/D%E1%BB%B1%20%C3%A1n%20Ti%E1%BA%BFng%20Vi%E1%BB%87t"
                ),
                ("My Project", "https://svn.example.com/svn/My%20Project"),
                ("MyRepo", "https://svn.example.com/svn/MyRepo"),
            ]
        );
        // Every URL parses back to the name it shows.
        for r in &repos {
            let u = url::Url::parse(&r.url).unwrap();
            let last = u.path_segments().unwrap().next_back().unwrap();
            assert_eq!(percent_decode(last).unwrap(), r.name);
        }
    }

    #[test]
    fn xml_index_gives_encoded_repository_urls() {
        let repos = repos_of("https://svn.example.com/svn", XML);
        let urls: Vec<&str> = repos.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://svn.example.com/svn/Hooked",
                "https://svn.example.com/svn/My%20Project",
                // The real name, even though only the printable part is shown.
                "https://svn.example.com/svn/ev%1Bil",
            ]
        );
        assert_eq!(repos[2].name, "evil");
    }

    #[test]
    fn unencoded_and_entity_escaped_names_are_encoded_once() {
        // VisualSVN writes the XML index with raw UTF-8 and `&amp;`; a page may also spell a
        // character without percent-encoding it.
        let page = r#"<?xml version="1.0" encoding="utf-8"?>
<?xml-stylesheet type="text/xsl" href="/svnindex.xsl"?>
<svn version="1.14.3"><index rev="0" path="Collection of Repositories">
  <dir name="Dự án" href="Dự%20án/" />
  <dir name="R&amp;D" href="R&amp;D/" />
  <dir name="100%" href="100%25/" />
  <dir name="a#b" href="a%23b/" />
  <dir name="dup" href="Dự%20án/" />
</index></svn>"#;
        let repos = repos_of("https://h:18443/svn", page);
        let got: Vec<(&str, &str)> = repos
            .iter()
            .map(|r| (r.name.as_str(), r.url.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("Dự án", "https://h:18443/svn/D%E1%BB%B1%20%C3%A1n"),
                ("R&D", "https://h:18443/svn/R&D"),
                ("100%", "https://h:18443/svn/100%25"),
                ("a#b", "https://h:18443/svn/a%23b"),
            ]
        );
        assert_eq!(
            parent_path_listing(page.as_bytes()).unwrap(),
            ["Dự án", "R&D", "100%", "a#b"]
        );
    }

    #[test]
    fn a_listing_at_the_server_root_keeps_one_slash() {
        let repos = repos_of("https://h/", HTML);
        assert_eq!(repos[2].url, "https://h/MyRepo");
    }

    #[test]
    fn markup_references_resolve_and_unknown_ones_stay() {
        assert_eq!(unescape_markup("a&amp;b&lt;&#39;&#x41;"), "a&b<'A");
        assert_eq!(unescape_markup("a&b; &nope; &"), "a&b; &nope; &");
    }

    #[test]
    fn request_adds_the_trailing_slash() {
        let req = parent_listing_request(&RepoUrl::parse("https://h/svn").unwrap()).unwrap();
        assert_eq!(req.url.as_str(), "https://h/svn/");
        assert_eq!(req.method, Method::Get);
    }
}
