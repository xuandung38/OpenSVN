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
