//! Error classification against recorded failure responses.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::load;
use proptest::prelude::*;
use svn_protocol::errors::parse_dav_error;
use svn_protocol::{error_from_response, map_http_status};
use svn_types::ErrorCode;

#[test]
fn out_of_date_put_is_classified_from_the_errcode() {
    let f = load("errors/out-of-date-409");
    assert_eq!(f.status, 409);
    let err = error_from_response(f.status, &f.resp_headers, &f.resp_body);
    assert_eq!(err.code, ErrorCode::OutOfDate);
    assert_eq!(err.svn_code, Some(170_004));
    assert_eq!(err.message, "File '/trunk/a.txt' is out of date");
}

#[test]
fn lock_conflict_chain() {
    let f = load("locks/lock-conflict-423");
    let err = error_from_response(f.status, &f.resp_headers, &f.resp_body);
    assert_eq!(
        (err.code, err.svn_code),
        (ErrorCode::LockedByOther, Some(160_035))
    );
}

#[test]
fn html_error_pages_fall_back_to_the_http_status() {
    let f = load("errors/auth-401-html");
    assert_eq!(
        error_from_response(f.status, &f.resp_headers, &f.resp_body).code,
        ErrorCode::AuthRequired
    );
    assert!(parse_dav_error(&f.resp_body).is_none());
    let f = load("errors/not-found-404-html");
    let err = error_from_response(f.status, &f.resp_headers, &f.resp_body);
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert!(err.svn_code.is_none());
    assert!(
        !err.message.contains("<html"),
        "the page body must not leak into the message"
    );
}

#[test]
fn status_fallback_table() {
    let table = [
        (401, ErrorCode::AuthRequired),
        (407, ErrorCode::AuthRequired),
        (403, ErrorCode::Forbidden),
        (404, ErrorCode::PathNotFound),
        (409, ErrorCode::OutOfDate),
        (423, ErrorCode::LockedByOther),
        (501, ErrorCode::ServerUnsupported),
        (502, ErrorCode::Network),
        (504, ErrorCode::Timeout),
        (500, ErrorCode::Internal),
        (418, ErrorCode::Internal),
    ];
    for (status, code) in table {
        assert_eq!(map_http_status(status), code, "{status}");
    }
}

#[test]
fn nested_error_chain_keeps_every_link() {
    let body = br#"<?xml version="1.0"?><D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns" xmlns:C="svn:"><C:error/>
<m:human-readable errcode="160013">outer</m:human-readable>
<m:human-readable errcode="175002">middle</m:human-readable>
<m:human-readable>inner without code</m:human-readable></D:error>"#;
    let err = error_from_response(404, &svn_protocol::Headers::new(), body);
    assert_eq!(err.code, ErrorCode::PathNotFound);
    let middle = err.child.as_ref().unwrap();
    assert_eq!(
        (middle.message.as_str(), middle.svn_code),
        ("middle", Some(175_002))
    );
    let inner = middle.child.as_ref().unwrap();
    assert_eq!(
        (inner.message.as_str(), inner.svn_code),
        ("inner without code", None)
    );
}

#[test]
fn oversized_or_malicious_chains_are_bounded() {
    let mut body =
        String::from(r#"<D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns">"#);
    for i in 0..1000 {
        body.push_str(&format!(
            r#"<m:human-readable errcode="{}">e{i}</m:human-readable>"#,
            160_000 + i
        ));
    }
    body.push_str("</D:error>");
    let entries = parse_dav_error(body.as_bytes()).unwrap();
    assert_eq!(entries.len(), 32);
}

proptest! {
    #[test]
    fn arbitrary_bodies_never_panic(status in 100u16..600, data in proptest::collection::vec(any::<u8>(), 0..300)) {
        let _ = error_from_response(status, &svn_protocol::Headers::new(), &data);
    }
}

#[test]
fn hook_failures_and_redirects_have_their_own_codes() {
    let body = br#"<D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns"><m:human-readable errcode="165001">pre-commit hook: no TODOs allowed</m:human-readable></D:error>"#;
    let e = error_from_response(500, &svn_protocol::Headers::new(), body);
    assert_eq!((e.code, e.svn_code), (ErrorCode::HookFailed, Some(165_001)));
    assert!(e.message.contains("no TODOs allowed"));

    let moved =
        svn_protocol::Headers::from_pairs([("Location", "https://svn.test.local/new/repo")]);
    for status in [301, 302, 307, 308] {
        let e = error_from_response(status, &moved, b"<html>moved</html>");
        assert_eq!(e.code, ErrorCode::RepositoryMoved, "{status}");
        assert_eq!(e.svn_code, Some(175_011));
        assert!(
            e.message.contains("https://svn.test.local/new/repo"),
            "{}",
            e.message
        );
    }
    // An unusable Location is not echoed.
    for loc in [
        "javascript:alert(1)",
        "ftp://x/y",
        "/relative",
        "https://a/b\nX-Evil: 1",
        "https://svn.test.local/alert/.\t./x",
        "https://svn.test.local\\alert",
        "https://svn.test.local/alert%0D%0AX-Evil:%201",
        "https://svn.test.local/alert%5C..",
        "",
    ] {
        let h = svn_protocol::Headers::from_pairs([("Location", loc)]);
        let e = error_from_response(302, &h, b"");
        assert_eq!(e.code, ErrorCode::RepositoryMoved);
        assert!(
            !e.message.contains("alert") && !e.message.contains("Evil"),
            "{}",
            e.message
        );
    }
    // A D:error body naming the relocation errcode maps the same way.
    let body = br#"<D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns"><m:human-readable errcode="175011">moved</m:human-readable></D:error>"#;
    assert_eq!(
        error_from_response(404, &svn_protocol::Headers::new(), body).code,
        ErrorCode::RepositoryMoved
    );
    assert_eq!(map_http_status(304), ErrorCode::Network);
}
