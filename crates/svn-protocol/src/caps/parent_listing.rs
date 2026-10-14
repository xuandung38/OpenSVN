//! A URL that is not a repository but the repository list of an Apache `SVNParentPath` location
//! (VisualSVN Server's `https://host/svn/`). mod_dav_svn answers the session's `OPTIONS` there
//! with a bare HTTP 500; a `GET` returns the list (`SVNListParentPath On`), as HTML or, with
//! `SVNIndexXSLT` (VisualSVN), as the XML index. Both carry the title
//! "Collection of Repositories" and one `href="<name>/"` per repository.

use svn_types::{ErrorCode, RepoUrl, Result, SvnError};

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

/// The repository names of a parent-path listing page, or `None` when `body` is not one.
pub fn parent_path_listing(body: &[u8]) -> Option<Vec<String>> {
    let text = String::from_utf8_lossy(body);
    if !text.contains("Collection of Repositories") {
        return None;
    }
    let mut names = Vec::new();
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("href=\"") {
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let href = &rest[..end];
        rest = &rest[end..];
        // Repository links are relative and end with '/'; skip the stylesheet, the project
        // link and `../`.
        let Some(name) = href.strip_suffix('/') else {
            continue;
        };
        if name.is_empty() || name.contains(['/', ':', '?', '#']) || name.starts_with('.') {
            continue;
        }
        if let Ok(decoded) = percent_decode(name) {
            let clean = sanitize_message(&decoded.replace(['\n', '\t'], " "), MAX_NAME_BYTES);
            if !clean.is_empty() && !names.contains(&clean) {
                names.push(clean);
            }
        }
    }
    Some(names)
}

/// The error for opening a parent-path listing as a repository.
pub fn parent_path_error(url: &RepoUrl, names: &[String]) -> SvnError {
    let base = url.as_url().as_str().trim_end_matches('/');
    let mut message = format!(
        "{base} is the list of repositories on this server, not a repository; add a \
         repository name to the URL"
    );
    if let Some(first) = names.first() {
        message.push_str(&format!(" (for example {base}/{first})"));
        let shown: Vec<&str> = names.iter().take(MAX_NAMES).map(String::as_str).collect();
        message.push_str(&format!(". Repositories here: {}", shown.join(", ")));
        if names.len() > MAX_NAMES {
            message.push_str(&format!(" and {} more", names.len() - MAX_NAMES));
        }
    }
    SvnError::new(ErrorCode::PathNotFound, message)
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

    #[test]
    fn error_names_the_repositories() {
        let url = RepoUrl::parse("https://svn.example.com/svn").unwrap();
        let names: Vec<String> = (0..12).map(|i| format!("r{i}")).collect();
        let err = parent_path_error(&url, &names);
        assert_eq!(err.code, ErrorCode::PathNotFound);
        assert!(
            err.message
                .contains("for example https://svn.example.com/svn/r0"),
            "{}",
            err.message
        );
        assert!(err.message.ends_with("r9 and 2 more"), "{}", err.message);
        let none = parent_path_error(&url, &[]);
        assert!(!none.message.contains("for example"), "{}", none.message);
    }

    #[test]
    fn request_adds_the_trailing_slash() {
        let req = parent_listing_request(&RepoUrl::parse("https://h/svn").unwrap()).unwrap();
        assert_eq!(req.url.as_str(), "https://h/svn/");
        assert_eq!(req.method, Method::Get);
    }
}
