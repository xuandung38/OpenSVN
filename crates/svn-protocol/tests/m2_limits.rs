//! Buffer limits of the update, file-revs and merge-response parsers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::urls;
use svn_protocol::commit::{MergeResponseParser, parse_merge_response};
use svn_protocol::file_revs::FileRevsParser;
use svn_protocol::update::UpdateParser;
use svn_protocol::{Headers, ParserLimits, ResponseHead, ResponseParser};
use svn_types::ErrorCode;

fn corrupt<T: std::fmt::Debug>(r: svn_types::Result<T>, what: &str) {
    assert_eq!(
        r.expect_err(what).code,
        ErrorCode::ProtocolViolation,
        "{what}"
    );
}

fn update_doc(inner: &str) -> String {
    format!(
        r#"<S:update-report xmlns:S="svn:" xmlns:D="DAV:" xmlns:V="http://subversion.tigris.org/xmlns/dav/"><S:open-directory rev="1">{inner}</S:open-directory></S:update-report>"#
    )
}

#[test]
fn update_report_value_caps() {
    let long = "x".repeat(9000);
    let href = update_doc(&format!(
        r#"<S:add-file name="f"><D:checked-in><D:href>/svn/repo/{long}</D:href></D:checked-in></S:add-file>"#
    ));
    corrupt(UpdateParser::new(urls()).feed(href.as_bytes()), "href");

    let small = ParserLimits {
        max_text_bytes: 1024,
        ..ParserLimits::default()
    };
    let prop = update_doc(&format!(
        r#"<S:set-prop name="p">{}</S:set-prop>"#,
        "v".repeat(2000)
    ));
    corrupt(
        UpdateParser::with_limits(urls(), small).feed(prop.as_bytes()),
        "property",
    );

    let md5 = update_doc(&format!(
        r#"<S:add-file name="f"><V:md5-checksum>{long}</V:md5-checksum></S:add-file>"#
    ));
    corrupt(UpdateParser::new(urls()).feed(md5.as_bytes()), "checksum");
}

#[test]
fn text_deltas_stream_without_a_size_cap() {
    // svndiff data is passed on in pieces, so even a tiny text limit must not stop it.
    let small = ParserLimits {
        max_text_bytes: 64,
        ..ParserLimits::default()
    };
    let mut p = UpdateParser::with_limits(urls(), small);
    p.feed(br#"<S:update-report xmlns:S="svn:"><S:open-directory rev="1"><S:add-file name="f"><S:txdelta>"#).unwrap();
    let mut total = 0;
    for _ in 0..64 {
        for e in p.feed("QUJD".repeat(16 * 1024).as_bytes()).unwrap() {
            if let svn_protocol::update::UpdateEvent::TxDeltaChunk { data, .. } = e {
                total += data.len();
            }
        }
    }
    assert!(total > 2 << 20, "{total}");
}

#[test]
fn file_revs_property_cap() {
    let small = ParserLimits {
        max_text_bytes: 100,
        ..ParserLimits::default()
    };
    let doc = format!(
        r#"<S:file-revs-report xmlns:S="svn:"><S:file-rev path="/a" rev="1"><S:rev-prop name="svn:log">{}</S:rev-prop></S:file-rev></S:file-revs-report>"#,
        "l".repeat(200)
    );
    corrupt(
        FileRevsParser::with_limits(small).feed(doc.as_bytes()),
        "rev-prop",
    );
}

#[test]
fn merge_response_caps_and_cleans_the_hook_message() {
    let small = ParserLimits {
        max_text_bytes: 100,
        ..ParserLimits::default()
    };
    let _ = small; // the parser takes the default limits; the oversized case uses the real cap below
    let big = format!(
        r#"<D:merge-response xmlns:D="DAV:"><D:updated-set><D:response><D:propstat><D:prop><D:resourcetype><D:baseline/></D:resourcetype><D:version-name>{}</D:version-name></D:prop></D:propstat></D:response></D:updated-set></D:merge-response>"#,
        "9".repeat(17 << 20)
    );
    corrupt(
        MergeResponseParser::new().feed(big.as_bytes()),
        "version field",
    );

    let doc = format!(
        r#"<D:merge-response xmlns:D="DAV:" xmlns:S="svn:"><D:updated-set><D:response><D:propstat><D:prop><D:resourcetype><D:baseline/></D:resourcetype><D:version-name>3</D:version-name></D:prop></D:propstat></D:response></D:updated-set><S:post-commit-err>{}</S:post-commit-err></D:merge-response>"#,
        "h".repeat(10_000)
    );
    let m = parse_merge_response(&ResponseHead::new(200, Headers::new()), doc.as_bytes()).unwrap();
    let msg = m.post_commit_error.unwrap();
    assert!(msg.len() <= 4 * 1024 + 3 && msg.ends_with('\u{2026}'));
}
