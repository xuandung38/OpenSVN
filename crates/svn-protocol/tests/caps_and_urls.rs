//! OPTIONS handling, URL construction and href vetting against recorded traffic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{caps, load, session_url, urls};
use svn_protocol::{BulkUpdates, Capabilities, HrefKind, UrlBuilder};
use svn_types::{ErrorCode, RelPath, Revnum};

#[test]
fn parses_recorded_options_response() {
    let c = caps();
    assert_eq!(c.me_resource, "/svn/repo/!svn/me");
    assert_eq!(c.repository_root, "/svn/repo");
    assert_eq!(c.rev_root_stub, "/svn/repo/!svn/rvr");
    assert_eq!(c.rev_stub, "/svn/repo/!svn/rev");
    assert_eq!(c.txn_root_stub.as_deref(), Some("/svn/repo/!svn/txr"));
    assert_eq!(c.txn_stub.as_deref(), Some("/svn/repo/!svn/txn"));
    assert_eq!(c.vtxn_root_stub.as_deref(), Some("/svn/repo/!svn/vtxr"));
    assert_eq!(c.bulk_updates, Some(BulkUpdates::Prefer));
    assert_eq!(c.repository_mergeinfo, Some(true));
    assert!(c.uuid.is_some());
    assert!(c.youngest_rev.is_some());
    assert_eq!(
        c.activity_collection.as_deref(),
        Some("/svn/repo/!svn/act/")
    );
    assert!(c.supports_post("create-txn") && c.supports_post("create-txn-with-props"));
    assert!(!c.supports_post("create-something-else"));
    let d = &c.dav;
    assert!(d.depth && d.mergeinfo && d.log_revprops && d.inline_props && d.list);
    assert!(d.svndiff1 && d.svndiff2 && d.put_result_checksum && d.reverse_file_revs);
}

#[test]
fn server_without_v2_headers_is_refused_with_a_clear_error() {
    let f = load("caps/options-200-no-v2-headers");
    let err =
        Capabilities::from_options_response(&session_url(), &f.head(), &f.resp_body).unwrap_err();
    assert_eq!(err.code, ErrorCode::ServerUnsupported);
    assert!(err.message.contains("HTTP protocol v2"), "{}", err.message);
}

#[test]
fn unauthorized_options_maps_to_auth_required() {
    let f = load("caps/options-401");
    let err =
        Capabilities::from_options_response(&session_url(), &f.head(), &f.resp_body).unwrap_err();
    assert_eq!(err.code, ErrorCode::AuthRequired);
}

#[test]
fn builds_urls_from_announced_stubs() {
    let u = urls();
    let path = RelPath::new("trunk/src").unwrap();
    assert_eq!(
        u.me().unwrap().as_str(),
        "http://svn.test.local/svn/repo/!svn/me"
    );
    assert_eq!(
        u.rev_root(Revnum(5), &path).unwrap().as_str(),
        "http://svn.test.local/svn/repo/!svn/rvr/5/trunk/src"
    );
    assert_eq!(
        u.rev_root(Revnum(0), &RelPath::root()).unwrap().as_str(),
        "http://svn.test.local/svn/repo/!svn/rvr/0"
    );
    assert_eq!(
        u.public(&path).unwrap().as_str(),
        "http://svn.test.local/svn/repo/trunk/src"
    );
    assert_eq!(
        u.rev(Revnum(7)).unwrap().as_str(),
        "http://svn.test.local/svn/repo/!svn/rev/7"
    );
    assert_eq!(
        u.txn_root("9-a", &path).unwrap().as_str(),
        "http://svn.test.local/svn/repo/!svn/txr/9-a/trunk/src"
    );
    assert_eq!(
        u.txn("9-a").unwrap().as_str(),
        "http://svn.test.local/svn/repo/!svn/txn/9-a"
    );
}

#[test]
fn path_segments_are_percent_encoded_once() {
    let u = urls();
    let p = RelPath::new("dir with space/caf\u{e9}%41").unwrap();
    assert_eq!(
        u.public(&p).unwrap().as_str(),
        "http://svn.test.local/svn/repo/dir%20with%20space/caf%C3%A9%2541"
    );
}

#[test]
fn bad_transaction_names_are_rejected() {
    let u = urls();
    for name in ["", "a/b", "..", "a b", "x?y"] {
        assert_eq!(
            u.txn(name).unwrap_err().code,
            ErrorCode::UnsafePath,
            "{name:?}"
        );
    }
}

#[test]
fn custom_stub_prefix_is_honoured_and_nothing_is_hard_coded() {
    let mut c = caps();
    c.me_resource = "/svn/repo/_dav/me".into();
    c.rev_root_stub = "/svn/repo/_dav/rvr".into();
    c.rev_stub = "/svn/repo/_dav/rev".into();
    c.txn_root_stub = Some("/svn/repo/_dav/txr".into());
    c.txn_stub = Some("/svn/repo/_dav/txn".into());
    let u = UrlBuilder::new(&session_url(), &c).unwrap();
    let url = u.rev_root(Revnum(3), &RelPath::new("a").unwrap()).unwrap();
    assert_eq!(url.as_str(), "http://svn.test.local/svn/repo/_dav/rvr/3/a");
    assert!(!url.as_str().contains("!svn"));
    // The old default prefix is now an unknown special-looking path inside the public tree.
    let accepted = u.accept_href("/svn/repo/_dav/rvr/3/a").unwrap();
    assert_eq!(
        accepted.kind,
        HrefKind::RevRoot {
            rev: Revnum(3),
            path: RelPath::new("a").unwrap()
        }
    );
}

#[test]
fn accepts_hrefs_below_known_prefixes() {
    let u = urls();
    let cases = [
        (
            "/svn/repo/!svn/rvr/4/trunk/docs",
            HrefKind::RevRoot {
                rev: Revnum(4),
                path: RelPath::new("trunk/docs").unwrap(),
            },
        ),
        (
            "/svn/repo/!svn/rvr/4/trunk/docs/",
            HrefKind::RevRoot {
                rev: Revnum(4),
                path: RelPath::new("trunk/docs").unwrap(),
            },
        ),
        (
            "/svn/repo/!svn/rvr/4",
            HrefKind::RevRoot {
                rev: Revnum(4),
                path: RelPath::root(),
            },
        ),
        (
            "http://svn.test.local/svn/repo/!svn/rev/9",
            HrefKind::Rev { rev: Revnum(9) },
        ),
        ("/svn/repo/!svn/me", HrefKind::Me),
        (
            "/svn/repo/!svn/txn/12-x",
            HrefKind::Txn {
                name: "12-x".into(),
            },
        ),
        (
            "/svn/repo/!svn/txr/12-x/trunk/a.txt",
            HrefKind::TxnRoot {
                name: "12-x".into(),
                path: RelPath::new("trunk/a.txt").unwrap(),
            },
        ),
        (
            "/svn/repo/trunk/a%20b.txt",
            HrefKind::Public {
                path: RelPath::new("trunk/a b.txt").unwrap(),
            },
        ),
        (
            "/svn/repo",
            HrefKind::Public {
                path: RelPath::root(),
            },
        ),
    ];
    for (href, kind) in cases {
        assert_eq!(u.accept_href(href).unwrap().kind, kind, "{href}");
    }
}

#[test]
fn rejects_hostile_hrefs() {
    let u = urls();
    let hostile = [
        "http://evil.test/svn/repo/!svn/rvr/4/a",
        "https://svn.test.local/svn/repo/trunk",
        "http://svn.test.local:8080/svn/repo/trunk",
        "//evil.test/svn/repo/trunk",
        "trunk/a.txt",
        "",
        "/etc/passwd",
        "/svn/other/trunk",
        "/svn/repo-evil/trunk",
        "/svn/repo/trunk/../../etc",
        "/svn/repo/trunk/%2e%2e/x",
        "/svn/repo/trunk/%2E%2E/x",
        "/svn/repo/trunk/./x",
        "/svn/repo/!svn/rvr/4/a/../../../../x",
        "/svn/repo/trunk/.svn/entries",
        "/svn/repo/trunk/.SVN",
        "/svn/repo/!svn/rvr/4/.svn/wc.db",
        "/svn/repo/trunk/a%5C..%5Cb",
        "/svn/repo/trunk/a\\b",
        "/svn/repo/trunk/a%2Fb/../c",
        "/svn/repo/trunk/C:/x",
        "/svn/repo/trunk/nul.txt",
        "/svn/repo/trunk/bad%00name",
        "/svn/repo/trunk/%zz",
        "/svn/repo/trunk//x",
        "/svn/repo/!svn/ver/3/trunk/a.txt",
        "/svn/repo/!svn/bc/4/trunk",
        "/svn/repo/!svn/rvr/notanumber/x",
        "/svn/repo/!svn/rvr/",
        "/svn/repo/!svn/txn/..",
        "/svn/repo/!svn/rev/4/extra",
        "/svn/repo/trunk?x=1",
        "/svn/repo/trunk#frag",
        "ftp://svn.test.local/svn/repo",
        "http://user:pw@svn.test.local/svn/repo/trunk",
        "http://svn.test.local@evil.test/svn/repo/trunk",
        "/svn/repo/trunk/a\nb",
    ];
    for href in hostile {
        let err = u.accept_href(href).expect_err(href);
        assert!(
            matches!(err.code, ErrorCode::UnsafePath | ErrorCode::Corrupt),
            "{href}: {:?}",
            err.code
        );
    }
}

#[test]
fn rejects_a_stub_on_another_origin() {
    let f = load("caps/options-200");
    let mut head = f.head();
    let mut headers = svn_protocol::Headers::new();
    for (k, v) in head.headers.iter() {
        if k.eq_ignore_ascii_case("SVN-Rev-Root-Stub") {
            headers.push(k, "http://evil.test/svn/repo/!svn/rvr");
        } else {
            headers.push(k, v);
        }
    }
    head.headers = headers;
    let err = Capabilities::from_options_response(&session_url(), &head, &f.resp_body).unwrap_err();
    assert_eq!(err.code, ErrorCode::UnsafePath);
}

/// Capabilities as `caps()` but with another `SVN-Repository-Root`.
fn caps_with_root(root: &str) -> Capabilities {
    let mut c = caps();
    c.repository_root = root.to_owned();
    c
}

#[test]
fn repository_root_must_be_an_ancestor_of_the_session_url() {
    let at = |u: &str| svn_types::RepoUrl::parse(u).unwrap();
    // The root itself and every URL below it are fine.
    for s in [
        "http://svn.test.local/svn/repo",
        "http://svn.test.local/svn/repo/trunk/src",
    ] {
        UrlBuilder::new(&at(s), &caps()).unwrap();
    }
    // A sibling, a parent or a name that merely shares the prefix is refused.
    for (s, root) in [
        ("http://svn.test.local/svn/repo", "/elsewhere"),
        ("http://svn.test.local/svn/repo", "/svn/repo/trunk"),
        ("http://svn.test.local/svn/repository/trunk", "/svn/repo"),
    ] {
        let err = UrlBuilder::new(&at(s), &caps_with_root(root)).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsafePath, "{s} vs {root}");
    }
}

#[test]
fn repository_root_with_other_case_is_accepted_for_visualsvn() {
    let session = svn_types::RepoUrl::parse("https://svn.test.local/svn/myrepo/Trunk").unwrap();
    let mut c = caps_with_root("/svn/MyRepo");
    c.me_resource = "/svn/MyRepo/!svn/me".into();
    let b = UrlBuilder::new(&session, &c).unwrap();
    let (rel, folded) = b.locate(session.as_url()).unwrap();
    assert!(folded);
    assert_eq!(rel.as_str(), "Trunk");
    assert_eq!(
        b.public(&rel).unwrap().as_str(),
        "https://svn.test.local/svn/MyRepo/Trunk"
    );
    // Only the root part folds; the rest keeps its case and a foreign root still fails.
    let other = url::Url::parse("https://svn.test.local/svn/other/x").unwrap();
    assert_eq!(b.locate(&other).unwrap_err().code, ErrorCode::UnsafePath);
    let foreign = url::Url::parse("https://evil.test.local/svn/MyRepo/x").unwrap();
    assert_eq!(b.locate(&foreign).unwrap_err().code, ErrorCode::UnsafePath);
}

#[test]
fn repository_root_with_dot_segments_is_refused() {
    let f = load("caps/options-200");
    for root in [
        "/svn/repo/..",
        "/svn/repo/./",
        "/svn/repo/%2e%2e",
        // The URL parser reads `\` as `/` for http(s), so these collapse just the same.
        "/svn/repo\\..",
        "/svn/repo\\.\\",
        "/svn/repo\\%2E%2E",
        "http://svn.test.local/svn/repo\\..",
    ] {
        let mut head = f.head();
        let mut headers = svn_protocol::Headers::new();
        for (k, v) in head.headers.iter() {
            if k.eq_ignore_ascii_case("SVN-Repository-Root") {
                headers.push(k, root);
            } else {
                headers.push(k, v);
            }
        }
        head.headers = headers;
        let err =
            Capabilities::from_options_response(&session_url(), &head, &f.resp_body).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsafePath, "{root}");
    }
}

#[test]
fn repository_root_case_fold_is_ascii_only() {
    let at = |u: &str| svn_types::RepoUrl::parse(u).unwrap();
    // ASCII case differences still fold (VisualSVN).
    let b = UrlBuilder::new(
        &at("http://svn.test.local/svn/k/trunk"),
        &caps_with_root("/svn/K"),
    )
    .unwrap();
    assert!(
        b.locate(at("http://svn.test.local/svn/k/trunk").as_url())
            .unwrap()
            .1
    );
    // Full Unicode lowercasing maps KELVIN SIGN (U+212A) to `k`, and other non-ASCII letters
    // to their lower case; none of these name the same repository, so all are refused.
    for (s, root) in [
        ("http://svn.test.local/svn/k/trunk", "/svn/%E2%84%AA"),
        ("http://svn.test.local/svn/%E2%84%AA/trunk", "/svn/k"),
        ("http://svn.test.local/svn/%C3%A9/trunk", "/svn/%C3%89"),
    ] {
        let err = UrlBuilder::new(&at(s), &caps_with_root(root)).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsafePath, "{s} vs {root}");
    }
}
