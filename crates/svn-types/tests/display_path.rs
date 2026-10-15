//! `RepoDisplayPath`: display-only repository paths.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use svn_types::{ErrorCode, RelPath, RepoDisplayPath};

#[test]
fn accepts_what_the_repository_can_contain() {
    for p in [
        "/",
        "/trunk/a.txt",
        "/trunk/aux.c",
        "/a:b",
        "/a\\b",
        "/.svn/x",
        "/COM1",
    ] {
        let d = RepoDisplayPath::parse(p).expect(p);
        assert_eq!(d.as_str(), p);
        assert_eq!(d.to_string(), p);
    }
}

#[test]
fn rejects_structurally_broken_paths() {
    assert_eq!(
        RepoDisplayPath::parse("trunk/a")
            .expect_err("relative")
            .code,
        ErrorCode::ProtocolViolation
    );
    for p in ["/a/../b", "/a/./b", "//a", "/a//b", "/a/", "/a\0b", "/a\nb"] {
        assert_eq!(
            RepoDisplayPath::parse(p).expect_err(p).code,
            ErrorCode::UnsafePath,
            "{p:?}"
        );
    }
}

#[test]
fn builds_from_a_relative_path_and_serializes_as_a_string() {
    let rel = RelPath::new("trunk/a.txt").expect("rel");
    let d = RepoDisplayPath::from_rel(&rel);
    assert_eq!(d.as_str(), "/trunk/a.txt");
    assert_eq!(serde_json::to_string(&d).expect("json"), "\"/trunk/a.txt\"");
    assert_eq!(RepoDisplayPath::from_rel(&RelPath::root()).as_str(), "/");
}

#[test]
fn leaving_display_form_is_an_explicit_validation() {
    let d = RepoDisplayPath::parse("/trunk/aux.c").expect("display");
    // The file system form must be requested explicitly and is stricter.
    assert!(RelPath::from_server(d.as_str().trim_start_matches('/')).is_err());
    let ok = RepoDisplayPath::parse("/trunk/a.c").expect("display");
    assert!(RelPath::from_server(ok.as_str().trim_start_matches('/')).is_ok());
}
