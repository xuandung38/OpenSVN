//! `RepoUrl` validation.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use svn_types::{RelPath, RepoUrl};

#[test]
fn rejects_embedded_credentials() {
    for u in [
        "https://user:pass@svn.test.local/repo",
        "https://user@svn.test.local/repo",
        "https://:pass@svn.test.local/repo",
    ] {
        assert!(RepoUrl::parse(u).is_err(), "{u} must be rejected");
    }
}

#[test]
fn normalizes_and_joins() {
    let u = RepoUrl::parse("https://svn.test.local/repo/").expect("url");
    assert_eq!(u.to_string(), "https://svn.test.local/repo");
    let j = u
        .join(&RelPath::new("trunk/a b.txt").expect("rel"))
        .expect("join");
    assert_eq!(j.to_string(), "https://svn.test.local/repo/trunk/a%20b.txt");
}

#[test]
fn rejects_other_schemes_query_and_fragment() {
    for u in [
        "file:///repo",
        "svn://host/r",
        "https://h/r?x=1",
        "https://h/r#f",
        "not a url",
    ] {
        assert!(RepoUrl::parse(u).is_err(), "{u} must be rejected");
    }
}
