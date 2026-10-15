//! Base64-encoded values, hard buffer limits, lock token extraction, error text and href
//! classification: behaviours a hostile or merely unusual server can trigger.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use common::{load, urls};
use svn_protocol::errors::{parse_dav_error, sanitize_message};
use svn_protocol::locks::{
    GetLocksParser, parse_get_lock_response, parse_get_locks, parse_lock_response,
};
use svn_protocol::log::{LogParser, parse_log};
use svn_protocol::propfind::MultiStatusParser;
use svn_protocol::xml::{Ns, XmlEvent, read_all};
use svn_protocol::{Headers, ParserLimits, ResponseHead, ResponseParser, error_from_response};
use svn_types::{ErrorCode, RelPath, Timestamp};

fn b64(s: &str) -> String {
    STANDARD.encode(s)
}

fn log_doc(item: &str) -> String {
    format!(
        r#"<S:log-report xmlns:S="svn:" xmlns:D="DAV:"><S:log-item>{item}</S:log-item></S:log-report>"#
    )
}

#[test]
fn base64_log_fields_are_decoded() {
    let date = "2026-10-13T08:44:26.336083Z";
    let doc = log_doc(&format!(
        r#"<D:version-name>5</D:version-name><S:date encoding="base64">{}</S:date><D:comment encoding="base64">{}</D:comment><D:creator-displayname encoding="base64">{}</D:creator-displayname>"#,
        b64(date),
        b64("hi\u{1b}[0m\nsecond line"),
        b64("al\u{1}ice"),
    ));
    let items = parse_log(doc.as_bytes()).unwrap();
    let e = &items[0].entry;
    assert_eq!(e.date, Some(svn_protocol::parse_timestamp(date).unwrap()));
    assert_eq!(e.message.as_deref(), Some("hi\u{1b}[0m\nsecond line"));
    assert_eq!(e.author.as_deref(), Some("al\u{1}ice"));
    // Whitespace inside base64 is ignored.
    let spaced = format!("{}\n{}", &b64(date)[..8], &b64(date)[8..]);
    let doc = log_doc(&format!(
        r#"<D:version-name>5</D:version-name><S:date encoding="base64">{spaced}</S:date>"#
    ));
    assert!(parse_log(doc.as_bytes()).unwrap()[0].entry.date.is_some());
}

#[test]
fn bad_encodings_are_protocol_errors() {
    for attr in [
        r#"encoding="rot13""#,
        r#"encoding="base64""#, // with invalid payload below
    ] {
        let doc = log_doc(&format!(
            r#"<D:version-name>5</D:version-name><D:comment {attr}>!!!not base64!!!</D:comment>"#
        ));
        assert_eq!(
            parse_log(doc.as_bytes()).unwrap_err().code,
            ErrorCode::ProtocolViolation,
            "{attr}"
        );
    }
    // Invalid UTF-8 is not an encoding error: svn serves such log messages and authors (loaded
    // with --bypass-prop-validation) and shows them escaped; the invalid bytes become U+FFFD.
    let not_utf8 = STANDARD.encode(b"bad \xff\xfe log");
    let doc = log_doc(&format!(
        r#"<D:version-name>5</D:version-name><D:comment encoding="base64">{not_utf8}</D:comment><D:creator-displayname encoding="base64">{not_utf8}</D:creator-displayname>"#
    ));
    let e = &parse_log(doc.as_bytes()).unwrap()[0].entry;
    assert_eq!(e.message.as_deref(), Some("bad \u{fffd}\u{fffd} log"));
    assert_eq!(e.author.as_deref(), Some("bad \u{fffd}\u{fffd} log"));
    // A date is still strict.
    let doc = log_doc(&format!(
        r#"<D:version-name>5</D:version-name><S:date encoding="base64">{not_utf8}</S:date>"#
    ));
    assert!(parse_log(doc.as_bytes()).is_err());
}

#[test]
fn recorded_log_with_control_characters_decodes() {
    let f = load("log/control-chars");
    assert!(
        String::from_utf8_lossy(&f.resp_body).contains(r#"encoding="base64""#),
        "server base64-encodes it"
    );
    let items = parse_log(&f.resp_body).unwrap();
    let msg = items[0].entry.message.as_deref().unwrap();
    assert!(msg.contains("\u{1b}[1mbold\u{1b}[0m"), "{msg:?}");
}

#[test]
fn base64_lock_fields_are_decoded() {
    let doc = format!(
        r#"<S:get-locks-report xmlns:S="svn:"><S:lock><S:path>/a</S:path><S:token>opaquelocktoken:1</S:token><S:creationdate>2026-10-13T08:44:30.014940Z</S:creationdate><S:owner encoding="base64">{}</S:owner><S:comment encoding="base64">{}</S:comment></S:lock></S:get-locks-report>"#,
        b64("bob"),
        b64("red \u{1b}[31mtext")
    );
    let locks = parse_get_locks(doc.as_bytes()).unwrap();
    assert_eq!(locks[0].owner, "bob");
    assert_eq!(locks[0].comment.as_deref(), Some("red \u{1b}[31mtext"));
}

#[test]
fn custom_revprops_are_returned_decoded() {
    let doc = log_doc(&format!(
        r#"<D:version-name>5</D:version-name><S:revprop name="custom:plain">v&lt;1&gt;</S:revprop><S:revprop name="custom:bin" encoding="base64">{}</S:revprop>"#,
        STANDARD.encode([0u8, 159, 1])
    ));
    let item = &parse_log(doc.as_bytes()).unwrap()[0];
    assert_eq!(
        item.revprops[0],
        ("custom:plain".to_owned(), Bytes::from_static(b"v<1>"))
    );
    assert_eq!(
        item.revprops[1],
        ("custom:bin".to_owned(), Bytes::from_static(&[0, 159, 1]))
    );
    let nameless = log_doc(r#"<D:version-name>5</D:version-name><S:revprop>x</S:revprop>"#);
    assert!(parse_log(nameless.as_bytes()).is_err());
}

// ---- limits -------------------------------------------------------------------------------

fn multistatus(prop_xml: &str) -> String {
    format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/svn/repo/!svn/rvr/1/a</D:href><D:propstat><D:prop>{prop_xml}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    )
}

fn run_multistatus(limits: ParserLimits, doc: &str) -> svn_types::Result<usize> {
    let mut p = MultiStatusParser::with_limits(limits);
    let mut n = p.feed(doc.as_bytes())?.len();
    n += p.finish()?.len();
    Ok(n)
}

fn assert_corrupt<T: std::fmt::Debug>(r: svn_types::Result<T>, what: &str) {
    let e = r.expect_err(what);
    assert_eq!(e.code, ErrorCode::ProtocolViolation, "{what}: {e}");
}

#[test]
fn href_and_status_are_capped_at_8_kib() {
    let long = "x".repeat(9000);
    let href = format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/{long}</D:href></D:response></D:multistatus>"#
    );
    assert_corrupt(run_multistatus(ParserLimits::default(), &href), "href");
    let status = format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/a</D:href><D:propstat><D:prop/><D:status>{long}</D:status></D:propstat></D:response></D:multistatus>"#
    );
    assert_corrupt(run_multistatus(ParserLimits::default(), &status), "status");
    let ok = format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/{}</D:href></D:response></D:multistatus>"#,
        "x".repeat(8000)
    );
    assert_eq!(run_multistatus(ParserLimits::default(), &ok).unwrap(), 1);
}

#[test]
fn property_element_count_is_capped_at_10k() {
    let many = |n: usize| {
        multistatus(&format!(
            "<D:resourcetype>{}</D:resourcetype>",
            "<a/>".repeat(n)
        ))
    };
    assert_eq!(
        run_multistatus(ParserLimits::default(), &many(10_000)).unwrap(),
        1
    );
    assert_corrupt(
        run_multistatus(ParserLimits::default(), &many(10_001)),
        "element count",
    );
}

#[test]
fn property_text_entry_and_property_counts_are_capped() {
    let small = ParserLimits {
        max_text_bytes: 1024,
        ..ParserLimits::default()
    };
    let doc = multistatus(&format!("<D:getetag>{}</D:getetag>", "x".repeat(2000)));
    assert_corrupt(run_multistatus(small, &doc), "text node");

    let few = ParserLimits {
        max_props_per_entry: 3,
        ..ParserLimits::default()
    };
    let four = multistatus("<D:a/><D:b/><D:c/><D:d/>");
    assert_corrupt(run_multistatus(few, &four), "props per response");
    assert_eq!(
        run_multistatus(few, &multistatus("<D:a/><D:b/><D:c/>")).unwrap(),
        1
    );

    let tight = ParserLimits {
        max_entry_bytes: 100,
        ..ParserLimits::default()
    };
    let doc = multistatus(&format!("<D:a>{0}</D:a><D:b>{0}</D:b>", "y".repeat(60)));
    assert_corrupt(run_multistatus(tight, &doc), "bytes per response");
}

#[test]
fn log_fields_are_capped() {
    let cap = ParserLimits {
        max_log_message_bytes: 100,
        ..ParserLimits::default()
    };
    let doc = log_doc(&format!(
        "<D:version-name>1</D:version-name><D:comment>{}</D:comment>",
        "m".repeat(200)
    ));
    assert_corrupt(
        LogParser::with_limits(cap).feed(doc.as_bytes()),
        "log message",
    );
    // The default allows large messages.
    let big = log_doc(&format!(
        "<D:version-name>1</D:version-name><D:comment>{}</D:comment>",
        "m".repeat(1 << 20)
    ));
    assert_eq!(
        parse_log(big.as_bytes()).unwrap()[0]
            .entry
            .message
            .as_ref()
            .unwrap()
            .len(),
        1 << 20
    );

    let author = log_doc(&format!(
        "<D:version-name>1</D:version-name><D:creator-displayname>{}</D:creator-displayname>",
        "a".repeat(9000)
    ));
    assert_corrupt(parse_log(author.as_bytes()), "author");
    let path = log_doc(&format!(
        r#"<S:added-path node-kind="file">/{}</S:added-path><D:version-name>1</D:version-name>"#,
        "p".repeat(9000)
    ));
    assert_corrupt(parse_log(path.as_bytes()), "path");

    let few = ParserLimits {
        max_changed_paths: 2,
        ..ParserLimits::default()
    };
    let paths: String = (0..3)
        .map(|i| format!(r#"<S:added-path node-kind="file">/p{i}</S:added-path>"#))
        .collect();
    let doc = log_doc(&format!("{paths}<D:version-name>1</D:version-name>"));
    assert_corrupt(
        LogParser::with_limits(few).feed(doc.as_bytes()),
        "changed paths",
    );

    let props = ParserLimits {
        max_prop_elements: 2,
        ..ParserLimits::default()
    };
    let rp: String = (0..3)
        .map(|i| format!(r#"<S:revprop name="p{i}">v</S:revprop>"#))
        .collect();
    let doc = log_doc(&format!("{rp}<D:version-name>1</D:version-name>"));
    assert_corrupt(
        LogParser::with_limits(props).feed(doc.as_bytes()),
        "revprops",
    );
}

#[test]
fn lock_report_buffers_only_known_fields_and_caps_them() {
    let small = ParserLimits {
        max_text_bytes: 1024,
        ..ParserLimits::default()
    };
    // 20 MiB inside an unknown element streams through without being kept or rejected.
    let mut p = GetLocksParser::with_limits(small);
    p.feed(br#"<S:get-locks-report xmlns:S="svn:"><S:lock><S:junk>"#)
        .unwrap();
    let chunk = vec![b'j'; 1 << 20];
    for _ in 0..20 {
        p.feed(&chunk).unwrap();
    }
    p.feed(b"</S:junk><S:path>/a</S:path><S:token>opaquelocktoken:1</S:token><S:owner>o</S:owner><S:creationdate>2026-10-13T08:44:30Z</S:creationdate></S:lock></S:get-locks-report>").unwrap();
    assert!(p.finish().unwrap().is_empty());

    let comment = format!(
        r#"<S:get-locks-report xmlns:S="svn:"><S:lock><S:comment>{}</S:comment></S:lock></S:get-locks-report>"#,
        "c".repeat(2000)
    );
    assert_corrupt(
        GetLocksParser::with_limits(small).feed(comment.as_bytes()),
        "lock comment",
    );
    let owner = format!(
        r#"<S:get-locks-report xmlns:S="svn:"><S:lock><S:owner>{}</S:owner></S:lock></S:get-locks-report>"#,
        "o".repeat(9000)
    );
    assert_corrupt(parse_get_locks(owner.as_bytes()), "lock owner");
}

#[test]
fn error_messages_are_truncated_and_cleaned() {
    let body = format!(
        r#"<D:error xmlns:D="DAV:" xmlns:m="http://apache.org/dav/xmlns"><m:human-readable errcode="160013">long{csi}[31m{text}</m:human-readable></D:error>"#,
        csi = '\u{9b}',
        text = "é".repeat(10_000)
    );
    let e = error_from_response(404, &Headers::new(), body.as_bytes());
    assert!(e.message.len() <= 4 * 1024 + 3, "{}", e.message.len());
    assert!(e.message.ends_with('\u{2026}'));
    assert!(!e.message.contains('\u{9b}'), "C1 control stripped");
    assert_eq!(e.code, ErrorCode::PathNotFound);
    // An oversized body is not inspected at all.
    let huge = body.replace(
        "</m:human-readable>",
        &format!("{}</m:human-readable>", "e".repeat(70_000)),
    );
    assert!(
        parse_dav_error(huge.as_bytes()).is_none(),
        "body beyond 64 KiB is not inspected"
    );
    assert_eq!(sanitize_message("a\u{1b}[0mb\tc\nd\0", 100), "a[0mb\tc\nd");
    assert_eq!(sanitize_message("abcdef", 4), "abcd\u{2026}");
}

// ---- lock tokens ----------------------------------------------------------------------------

fn lock_head() -> ResponseHead {
    ResponseHead::new(
        200,
        Headers::from_pairs([
            ("X-SVN-Lock-Owner", "alice"),
            ("X-SVN-Creation-Date", "2026-10-13T08:44:30.014940Z"),
        ]),
    )
}

fn lock_body(owner: &str, token: &str) -> String {
    format!(
        r#"<D:prop xmlns:D="DAV:"><D:lockdiscovery><D:activelock><D:locktype><D:write/></D:locktype><D:lockscope><D:exclusive/></D:lockscope><D:depth>0</D:depth>{owner}<D:timeout>Infinite</D:timeout><D:locktoken><D:href>{token}</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop>"#
    )
}

#[test]
fn token_comes_only_from_locktoken_href() {
    let owner = "<D:owner><D:href>mailto:bob@svn.test.local</D:href></D:owner>";
    let path = RelPath::new("a").unwrap();
    let lock = parse_lock_response(
        &path,
        &lock_head(),
        lock_body(owner, "opaquelocktoken:1234").as_bytes(),
    )
    .unwrap();
    assert_eq!(lock.token, "opaquelocktoken:1234");

    let multistatus = format!(
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/svn/repo/a</D:href><D:propstat><D:prop><D:lockdiscovery><D:activelock>{owner}<D:locktoken><D:href>opaquelocktoken:abcd</D:href></D:locktoken></D:activelock></D:lockdiscovery></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#
    );
    let lock = parse_get_lock_response(&path, &lock_head(), multistatus.as_bytes())
        .unwrap()
        .unwrap();
    assert_eq!(lock.token, "opaquelocktoken:abcd");
}

#[test]
fn malformed_tokens_are_rejected_at_parse_time() {
    let path = RelPath::new("a").unwrap();
    for token in [
        "opaquelocktoken:1\nX-Evil: 1",
        "opaquelocktoken:a b",
        "mailto:bob@example.com",
        "opaquelocktoken:",
        "",
    ] {
        let err =
            parse_lock_response(&path, &lock_head(), lock_body("", token).as_bytes()).unwrap_err();
        assert_eq!(err.code, ErrorCode::LockTokenInvalid, "{token:?}");
    }
    let too_long = format!("opaquelocktoken:{}", "z".repeat(300));
    assert!(parse_lock_response(&path, &lock_head(), lock_body("", &too_long).as_bytes()).is_err());
    let report = r#"<S:get-locks-report xmlns:S="svn:"><S:lock><S:path>/a</S:path><S:token>evil token</S:token><S:owner>o</S:owner><S:creationdate>2026-10-13T08:44:30Z</S:creationdate></S:lock></S:get-locks-report>"#;
    assert_eq!(
        parse_get_locks(report.as_bytes()).unwrap_err().code,
        ErrorCode::LockTokenInvalid
    );
}

// ---- hrefs, attributes, XML strictness --------------------------------------------------------

#[test]
fn special_resource_guard_survives_percent_encoding() {
    let u = urls();
    for href in [
        "/svn/repo/%21svn/ver/3/trunk/a.txt",
        "/svn/repo/%21svn/bc/3/trunk",
        "/svn/repo/!svn/%76er/3/x",
        "/svn/repo/%21svn/rvr/3/%2e%73vn/x",
        "/svn/repo/%2521svn/x/%2F",
    ] {
        let r = u.accept_href(href);
        let is_bad = !matches!(r, Ok(ref a) if matches!(&a.kind, svn_protocol::HrefKind::RevRoot { .. } | svn_protocol::HrefKind::Public { .. }) && !href.contains("%2521"));
        assert!(is_bad || href.contains("%2521"), "{href}: {r:?}");
    }
    assert!(u.accept_href("/svn/repo/%21svn/ver/3/trunk/a.txt").is_err());
    assert!(u.accept_href("/svn/repo/!svn/%76er/3/x").is_err());
    // The encoded spelling of a known stub is the same resource.
    assert!(matches!(
        u.accept_href("/svn/repo/%21svn/rvr/3/trunk").unwrap().kind,
        svn_protocol::HrefKind::RevRoot { .. }
    ));
}

#[test]
fn attributes_are_matched_by_namespace() {
    let events =
        read_all(br#"<a xmlns:x="urn:x" name="plain" x:name="foreign" x:only="1"/>"#).unwrap();
    let XmlEvent::Start(tag) = &events[0] else {
        panic!()
    };
    assert_eq!(tag.attr("name"), Some("plain"));
    assert_eq!(
        tag.attr("only"),
        None,
        "a prefixed attribute is not an unprefixed one"
    );
    assert_eq!(
        tag.attr_in(&Ns::Other("urn:x".into()), "name"),
        Some("foreign")
    );
}

#[test]
fn xml_reader_is_as_strict_as_expat_about_characters() {
    let rejected: [&[u8]; 6] = [
        b"<a>&#1;</a>",
        b"<a>\x01</a>",
        b"<a b='\x02'/>",
        b"<a b='x<y'/>",
        b"<a><?xml version='1.0'?></a>",
        b"<a>&#xFFFE;</a>",
    ];
    for doc in rejected {
        assert!(read_all(doc).is_err(), "{:?}", String::from_utf8_lossy(doc));
    }
    let ok =
        read_all("<?xml version='1.0'?><a b='t\tn\n'>tab\tnl\ncr&#13;</a>".as_bytes()).unwrap();
    assert!(
        ok.iter()
            .any(|e| matches!(e, XmlEvent::Text(t) if t.contains('\t')))
    );
    // A declaration after a split-off leading comment is just as late.
    assert!(read_all(b"<!-- c --><?xml version='1.0'?><a/>").is_err());
}

#[test]
fn date_values_are_validated() {
    assert!(svn_protocol::parse_timestamp("2026-10-13T08:44:26.336083Z").unwrap() > Timestamp(0));
}
