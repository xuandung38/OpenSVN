//! VisualSVN web-interface link conversion.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use svn_types::{RepoUrl, Revnum, is_visualsvn_web_link, parse_visualsvn_web_link};

fn convert(s: &str) -> (String, Option<u64>) {
    let link = parse_visualsvn_web_link(s)
        .unwrap_or_else(|| panic!("{s} must be recognised"))
        .unwrap_or_else(|e| panic!("{s} must convert: {e}"));
    (link.repo_url.to_string(), link.peg_rev.map(|r| r.0))
}

#[test]
fn converts_the_documented_forms() {
    for (link, url, rev) in [
        (
            "https://svn.example.com/!/#Repo",
            "https://svn.example.com/svn/Repo",
            None,
        ),
        (
            "https://svn.example.com/!/#Repo/",
            "https://svn.example.com/svn/Repo",
            None,
        ),
        (
            "https://svn.example.com/!/#Repo/view/head/trunk/src",
            "https://svn.example.com/svn/Repo/trunk/src",
            None,
        ),
        (
            "https://svn.example.com/!/#Repo/view/HEAD/trunk/",
            "https://svn.example.com/svn/Repo/trunk",
            None,
        ),
        (
            "https://svn.example.com/!/#Repo/view/r1234/branches/x",
            "https://svn.example.com/svn/Repo/branches/x",
            Some(1234),
        ),
        (
            "https://svn.example.com:8443/!/#Repo/view/head/trunk",
            "https://svn.example.com:8443/svn/Repo/trunk",
            None,
        ),
        (
            "http://svn.example.com:8080/!/#Repo",
            "http://svn.example.com:8080/svn/Repo",
            None,
        ),
        (
            "https://svn.example.com/vsvn/!/#Repo/view/r7/trunk",
            "https://svn.example.com/vsvn/svn/Repo/trunk",
            Some(7),
        ),
        (
            "https://svn.example.com:8443/a/b/!/#Repo",
            "https://svn.example.com:8443/a/b/svn/Repo",
            None,
        ),
        // Without the trailing slash after `!`.
        (
            "https://svn.example.com/!#Repo",
            "https://svn.example.com/svn/Repo",
            None,
        ),
        // Surrounding whitespace from a paste.
        (
            "  https://svn.example.com/!/#Repo\n",
            "https://svn.example.com/svn/Repo",
            None,
        ),
    ] {
        assert_eq!(convert(link), (url.to_owned(), rev), "{link}");
    }
}

#[test]
fn other_pages_and_bare_view_give_the_repository_root() {
    for link in [
        "https://svn.example.com/!/#Repo/view",
        "https://svn.example.com/!/#Repo/history/head/trunk",
        "https://svn.example.com/!/#Repo/view/head/?tab=log",
    ] {
        assert_eq!(
            convert(link).0,
            "https://svn.example.com/svn/Repo",
            "{link}"
        );
    }
}

#[test]
fn decodes_and_re_encodes_names() {
    // Dots, spaces and Vietnamese, typed raw or already percent-encoded.
    let (url, _) = convert("https://svn.example.com/!/#Dự án.Web/view/head/trunk/thư mục a");
    assert_eq!(
        url,
        "https://svn.example.com/svn/D%E1%BB%B1%20%C3%A1n.Web/trunk/th%C6%B0%20m%E1%BB%A5c%20a"
    );
    let (again, _) = convert(
        "https://svn.example.com/!/#D%E1%BB%B1%20%C3%A1n.Web/view/head/trunk/th%C6%B0%20m%E1%BB%A5c%20a",
    );
    assert_eq!(again, url);
    // Characters that are special in a path are encoded, not interpreted.
    let (url, _) = convert("https://svn.example.com/!/#Repo/view/head/a%23b/c%3Fd/50%25");
    assert_eq!(url, "https://svn.example.com/svn/Repo/a%23b/c%3Fd/50%25");
    // The result is a valid repository URL as is.
    RepoUrl::parse(&url).expect("valid repository URL");
}

#[test]
fn refuses_path_escapes_and_bad_text() {
    for link in [
        "https://svn.example.com/!/#..",
        "https://svn.example.com/!/#Repo/view/head/../../etc",
        "https://svn.example.com/!/#Repo/view/head/%2e%2e/x",
        "https://svn.example.com/!/#Repo/view/head/a%2Fb",
        "https://svn.example.com/!/#Repo/view/head/a%5Cb",
        "https://svn.example.com/!/#Repo/view/head/a%0Ab",
        "https://svn.example.com/!/#Repo%00",
        "https://svn.example.com/!/#Repo/view/head/%ZZ",
        "https://svn.example.com/!/#Repo/view/head/%FF",
        "https://svn.example.com/!/#Repo/view/latest/trunk",
        "https://svn.example.com/!/#Repo/view/r/trunk",
        "https://svn.example.com/!/#Repo/view/r12x/trunk",
        "https://svn.example.com/!/#",
        "https://svn.example.com/!/#/",
        "https://svn.example.com/!/?x=1#Repo",
        "https://user:pw@svn.example.com/!/#Repo",
        "https://user@svn.example.com/!/#Repo",
    ] {
        let r = parse_visualsvn_web_link(link).unwrap_or_else(|| panic!("{link} is a web link"));
        assert!(r.is_err(), "{link} must be refused, got {r:?}");
    }
}

#[test]
fn keeps_the_origin_the_user_typed() {
    // A fragment cannot move the candidate to another host or port.
    for link in [
        "https://svn.example.com/!/#@other.example.test",
        "https://svn.example.com/!/#%2F%2Fother.example.test/x",
        "https://svn.example.com/!/#Repo/view/head/https:%2F%2Fother.example.test",
    ] {
        if let Some(Ok(l)) = parse_visualsvn_web_link(link) {
            assert_eq!(
                l.repo_url.as_url().host_str(),
                Some("svn.example.com"),
                "{link}"
            );
            assert_eq!(
                l.repo_url.as_url().port_or_known_default(),
                Some(443),
                "{link}"
            );
        }
    }
}

#[test]
fn ignores_urls_that_are_not_web_links() {
    for s in [
        "https://svn.example.com/svn/Repo",
        "https://svn.example.com/svn/Repo#frag",
        "https://svn.example.com/svn/Repo?p=1",
        "https://svn.example.com/docs/#section",
        "https://svn.example.com/a!/#Repo",
        "https://svn.example.com/!/x#Repo",
        "https://svn.example.com/!/",
        "svn://svn.example.com/!/#Repo",
        "file:///!/#Repo",
        "not a url",
        "",
    ] {
        assert!(!is_visualsvn_web_link(s), "{s}");
        assert!(parse_visualsvn_web_link(s).is_none(), "{s}");
    }
}

#[test]
fn repo_url_errors_explain_the_expected_form() {
    let web = RepoUrl::parse("https://svn.example.com/!/#Repo").unwrap_err();
    assert!(
        web.message.contains("VisualSVN web-interface link"),
        "{web}"
    );
    assert!(web.message.contains("Copy URL to clipboard"), "{web}");
    for s in [
        "https://svn.example.com/svn/Repo#frag",
        "https://svn.example.com/svn/Repo?p=1",
    ] {
        let e = RepoUrl::parse(s).unwrap_err();
        assert!(e.message.contains("no query (?) or fragment (#)"), "{e}");
        assert!(
            e.message.contains("https://svn.example.com/svn/Repo/trunk"),
            "{e}"
        );
    }
}

#[test]
fn peg_revision_type() {
    let l = parse_visualsvn_web_link("https://svn.example.com/!/#R/view/r0/trunk")
        .unwrap()
        .unwrap();
    assert_eq!(l.peg_rev, Some(Revnum(0)));
}
