//! PROPFIND parsing against recorded mod_dav_svn responses.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{DATA_SETS, for_each_set, load, run_split, urls, use_set};
use proptest::prelude::*;
use svn_protocol::ResponseParser;
use svn_protocol::propfind::{ListItem, ListParser, MultiStatusParser, parse_list, parse_stat};
use svn_protocol::xml::Ns;
use svn_types::{ErrorCode, NodeKind, RelPath, Revnum};

#[test]
fn stat_of_a_directory() {
    for_each_set(|| {
        let f = load("propfind/stat-dir");
        let e = parse_stat(&urls(), &f.resp_body).unwrap();
        assert_eq!(e.name, "src");
        assert_eq!(e.kind, NodeKind::Dir);
        assert_eq!(e.size, None);
        assert_eq!(e.created_rev, Some(Revnum(2)));
        assert_eq!(e.last_author.as_deref(), Some("alice"));
        assert!(e.time.is_some());
        assert!(!e.has_props);
    });
}

#[test]
fn stat_of_a_file_with_properties() {
    for_each_set(|| {
        let f = load("propfind/stat-file");
        let e = parse_stat(&urls(), &f.resp_body).unwrap();
        assert_eq!(e.name, "a.txt");
        assert_eq!(e.kind, NodeKind::File);
        assert_eq!(e.size, Some(17));
        assert_eq!(e.created_rev, Some(Revnum(3)));
        assert!(e.has_props, "deadprop-count is 1");
    });
}

#[test]
fn missing_node_is_reported_by_status_not_by_parsing_html() {
    for_each_set(|| {
        let f = load("propfind/stat-missing-404");
        assert_eq!(f.status, 404);
        let err = svn_protocol::error_from_response(f.status, &f.resp_headers, &f.resp_body);
        assert_eq!(err.code, ErrorCode::PathNotFound);
        assert!(
            parse_stat(&urls(), &f.resp_body).is_err(),
            "an HTML page is not a multistatus"
        );
    });
}

#[test]
fn listing_returns_the_directory_and_its_children() {
    for_each_set(|| {
        let f = load("propfind/list-depth1");
        let target = RelPath::new("trunk").unwrap();
        let (dir, children, rejected) = parse_list(&urls(), &target, &f.resp_body).unwrap();
        assert_eq!(dir.name, "trunk");
        assert_eq!(dir.kind, NodeKind::Dir);
        let mut names: Vec<_> = children.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable(); // the server answers in hash order
        assert_eq!(names, ["a.txt", "docs", "src", "unicod\u{e9} file.txt"]);
        let a = children.iter().find(|c| c.name == "a.txt").unwrap();
        assert_eq!(
            (a.kind, a.size, a.created_rev),
            (NodeKind::File, Some(17), Some(Revnum(3)))
        );
        assert!(
            children
                .iter()
                .filter(|c| c.kind == NodeKind::Dir)
                .all(|c| c.size.is_none())
        );
        assert!(rejected.is_empty());
    });
}

#[test]
fn listing_names_are_nfc_and_unicode_survives() {
    for_each_set(|| {
        let f = load("propfind/list-depth1");
        let target = RelPath::new("trunk").unwrap();
        let (_, children, _) = parse_list(&urls(), &target, &f.resp_body).unwrap();
        assert!(children.iter().any(|c| c.name == "unicod\u{e9} file.txt"));
    });
}

#[test]
fn listing_of_the_wrong_directory_is_corrupt() {
    for_each_set(|| {
        let f = load("propfind/list-depth1");
        let err =
            parse_list(&urls(), &RelPath::new("branches").unwrap(), &f.resp_body).unwrap_err();
        assert_eq!(err.code, ErrorCode::Corrupt);
    });
}

#[test]
fn hostile_names_are_rejected_individually_not_silently() {
    for_each_set(|| {
        let body = br#"<?xml version="1.0"?>
    <D:multistatus xmlns:D="DAV:">
    <D:response><D:href>/svn/repo/!svn/rvr/5/trunk/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    <D:response><D:href>/svn/repo/!svn/rvr/5/trunk/ok.txt</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    <D:response><D:href>/svn/repo/!svn/rvr/5/trunk/.svn</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    <D:response><D:href>http://evil.test/svn/repo/!svn/rvr/5/trunk/x</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    <D:response><D:href>/svn/repo/!svn/rvr/5/trunk/%2e%2e/%2e%2e/etc</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    <D:response><D:href>/svn/repo/!svn/rvr/5/trunk/CON</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
    </D:multistatus>"#;
        let (_, children, rejected) =
            parse_list(&urls(), &RelPath::new("trunk").unwrap(), body).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, "ok.txt");
        assert_eq!(rejected.len(), 4);
        assert!(
            rejected
                .iter()
                .all(|r| r.error.code == ErrorCode::UnsafePath)
        );
    });
}

#[test]
fn allprop_exposes_versioned_custom_and_dav_properties() {
    for_each_set(|| {
        let f = load("propfind/allprop-file");
        let mut p = MultiStatusParser::new();
        let mut entries = p.feed(&f.resp_body).unwrap();
        entries.extend(p.finish().unwrap());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        let props = e.versioned_props();
        let custom = props
            .iter()
            .find(|(n, _)| n == "custom:tag")
            .expect("custom property");
        assert_eq!(custom.1, br#"v<1>&"q""#);
        assert_eq!(e.get(&Ns::SvnDav, "md5-checksum").unwrap().text().len(), 32);
        assert_eq!(e.get(&Ns::Dav, "getcontentlength").unwrap().text(), "17");
        assert_eq!(
            e.get(&Ns::Dav, "checked-in").unwrap().descendant_at("href"),
            Some("/svn/repo/!svn/ver/3/trunk/a.txt")
        );
        assert!(
            e.get(&Ns::Dav, "lockdiscovery")
                .unwrap()
                .child_names
                .is_empty()
        );
    });
}

#[test]
fn base64_encoded_property_values_are_decoded() {
    let body = br#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:S="http://subversion.tigris.org/xmlns/svn/" xmlns:V="http://subversion.tigris.org/xmlns/dav/">
<D:response><D:href>/svn/repo/!svn/rvr/5/trunk/a</D:href><D:propstat><D:prop>
<S:ignore V:encoding="base64">Zm9v
YmFy</S:ignore>
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
    let mut p = MultiStatusParser::new();
    let mut entries = p.feed(body).unwrap();
    entries.extend(p.finish().unwrap());
    assert_eq!(
        entries[0].versioned_props(),
        vec![("svn:ignore".to_owned(), b"foobar".to_vec())]
    );
    let bad = String::from_utf8_lossy(body).replace("Zm9v", "!!!!");
    let mut p = MultiStatusParser::new();
    assert!(p.feed(bad.as_bytes()).is_err());
}

#[test]
fn not_found_propstat_is_recorded_as_missing() {
    for_each_set(|| {
        let f = load("propfind/stat-dir");
        let mut p = MultiStatusParser::new();
        let mut entries = p.feed(&f.resp_body).unwrap();
        entries.extend(p.finish().unwrap());
        assert!(
            entries[0]
                .missing
                .iter()
                .any(|n| n.local == "getcontentlength")
        );
        assert!(entries[0].get(&Ns::Dav, "getcontentlength").is_none());
    });
}

#[test]
fn rejects_documents_that_are_not_multistatus() {
    let mut p = MultiStatusParser::new();
    assert!(p.feed(b"<html><body>hi</body></html>").is_err());
}

proptest! {
    #[test]
    fn listing_is_independent_of_chunking(cuts in proptest::collection::vec(0usize..8192, 0..10)) {
        for set in DATA_SETS {
            use_set(set);
            let f = load("propfind/list-depth1");
            let u = urls();
            let target = RelPath::new("trunk").unwrap();
            let whole = run_split(ListParser::new(&u, target.clone()), &f.resp_body, &[]);
            let split = run_split(ListParser::new(&u, target), &f.resp_body, &cuts);
            let names = |items: &[ListItem]| -> Vec<String> {
                items.iter().map(|i| match i {
                    ListItem::Dir(d) | ListItem::Entry(d) => d.name.clone(),
                    ListItem::Rejected(r) => r.href.clone(),
                }).collect()
            };
            prop_assert_eq!(names(&whole), names(&split));
        }
    }

    #[test]
    fn allprop_is_independent_of_chunking(cuts in proptest::collection::vec(0usize..8192, 0..10)) {
        for set in DATA_SETS {
            use_set(set);
            let f = load("propfind/allprop-file");
            let whole = run_split(MultiStatusParser::new(), &f.resp_body, &[]);
            let split = run_split(MultiStatusParser::new(), &f.resp_body, &cuts);
            prop_assert_eq!(whole, split);
        }
    }
}
