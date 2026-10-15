//! `get-location-segments` request and response (bodies recorded from mod_dav_svn 1.14.3 and
//! 1.15.0, which answer the same).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use svn_protocol::location_segments::{
    LocationSegment, LocationSegmentsParser, location_segments_request, parse_location_segments,
};
use svn_protocol::{ResponseParser, parse_chunked};
use svn_types::{ErrorCode, RelPath, Revnum};

const BRANCH: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<S:get-location-segments-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
<S:location-segment path=\"branches/b/a&amp;b &lt;c&gt;\" range-start=\"3\" range-end=\"4\"/>\n\
<S:location-segment path=\"trunk/a&amp;b &lt;c&gt;\" range-start=\"1\" range-end=\"2\"/>\n\
</S:get-location-segments-report>\n";

const ROOT: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<S:get-location-segments-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
<S:location-segment path=\"\" range-start=\"0\" range-end=\"4\"/>\n\
</S:get-location-segments-report>\n";

fn text(s: &LocationSegment) -> String {
    format!(
        "{}:{}-{}",
        s.path.as_ref().map_or("<gap>", |p| p.as_str()),
        s.range_start.0,
        s.range_end.0
    )
}

#[test]
fn segments_of_a_copied_node_youngest_first() {
    let segs: Vec<String> = parse_location_segments(BRANCH)
        .unwrap()
        .iter()
        .map(text)
        .collect();
    assert_eq!(segs, ["/branches/b/a&b <c>:3-4", "/trunk/a&b <c>:1-2"]);
    // Any chunking gives the same.
    for chunk in [1, 7, 64] {
        let again = parse_chunked(&mut LocationSegmentsParser::new(), BRANCH, chunk).unwrap();
        assert_eq!(
            again.iter().map(text).collect::<Vec<_>>(),
            segs,
            "chunk {chunk}"
        );
    }
}

#[test]
fn the_root_and_gaps_and_absolute_paths() {
    let segs: Vec<String> = parse_location_segments(ROOT)
        .unwrap()
        .iter()
        .map(text)
        .collect();
    assert_eq!(segs, ["/:0-4"]);
    let body = b"<S:get-location-segments-report xmlns:S=\"svn:\">\
        <S:location-segment path=\"/b\" range-start=\"5\" range-end=\"9\"/>\
        <S:location-segment range-start=\"3\" range-end=\"4\"/>\
        <S:location-segment path=\"/a\" range-start=\"1\" range-end=\"2\"/>\
        </S:get-location-segments-report>";
    let segs: Vec<String> = parse_location_segments(body)
        .unwrap()
        .iter()
        .map(text)
        .collect();
    assert_eq!(segs, ["/b:5-9", "<gap>:3-4", "/a:1-2"]);
}

#[test]
fn malformed_answers_are_corrupt() {
    for body in [
        &b"<S:log-report xmlns:S=\"svn:\"/>"[..],
        b"<S:get-location-segments-report xmlns:S=\"svn:\">\
          <S:location-segment path=\"a\" range-start=\"4\" range-end=\"3\"/>\
          </S:get-location-segments-report>",
        b"<S:get-location-segments-report xmlns:S=\"svn:\">\
          <S:location-segment path=\"a\" range-end=\"3\"/></S:get-location-segments-report>",
        b"<S:get-location-segments-report xmlns:S=\"svn:\">\
          <S:location-segment path=\"a\" range-start=\"x\" range-end=\"3\"/>\
          </S:get-location-segments-report>",
        b"<S:get-location-segments-report xmlns:S=\"svn:\">",
    ] {
        let err = parse_location_segments(body).unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Corrupt,
            "{}",
            String::from_utf8_lossy(body)
        );
    }
    let traversal = b"<S:get-location-segments-report xmlns:S=\"svn:\">\
        <S:location-segment path=\"a/../b\" range-start=\"1\" range-end=\"3\"/>\
        </S:get-location-segments-report>";
    assert!(parse_location_segments(traversal).is_err());
    // A parser that failed stays failed.
    let mut p = LocationSegmentsParser::new();
    assert!(p.feed(b"<S:other xmlns:S=\"svn:\">").is_err());
    assert!(p.finish().is_err());
}

#[test]
fn request_asks_the_whole_history_at_the_peg_revision_url() {
    let urls = common::urls();
    let req = location_segments_request(&urls, &RelPath::new("branches/a b").unwrap(), Revnum(7))
        .unwrap();
    let rev_root = urls.rev_root(Revnum(7), &RelPath::root()).unwrap();
    assert_eq!(
        req.url.as_str(),
        format!("{}/branches/a%20b", rev_root.as_str().trim_end_matches('/'))
    );
    assert!(req.url.path().contains("/!svn/rvr/7/"), "{}", req.url);
    let body = String::from_utf8(req.body_bytes().unwrap().to_vec()).unwrap();
    assert!(
        body.contains("<S:get-location-segments xmlns:S=\"svn:\">"),
        "{body}"
    );
    for part in [
        "<S:path></S:path>",
        "<S:peg-revision>7</S:peg-revision>",
        "<S:start-revision>7</S:start-revision>",
        "<S:end-revision>0</S:end-revision>",
    ] {
        assert!(body.contains(part), "{part} in {body}");
    }
}
