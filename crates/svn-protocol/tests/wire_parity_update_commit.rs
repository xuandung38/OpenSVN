//! Update, commit and blame builders must match what svn 1.15 sent.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use bytes::Bytes;
use common::{Fixture, canonical_xml, caps, load, urls};
use svn_protocol::HttpRequest;
use svn_protocol::commit::{
    LockHandling, LockToken, MergeOptions, PutOptions, abort_txn_request, copy_request,
    create_txn_request, delete_request, merge_request, mkcol_request, proppatch_request,
    put_request,
};
use svn_protocol::file_revs::file_revs_request;
use svn_protocol::update::{UpdateMode, UpdateReportBuilder, UpdateRequestOptions};
use svn_types::{Depth, PropChange, RelPath, Revnum};

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

fn same_headers(req: &HttpRequest, f: &Fixture, name: &str) {
    let transport_owned = [
        "user-agent",
        "accept",
        "host",
        "authorization",
        "content-length",
        "transfer-encoding",
    ];
    for (k, _) in req.headers.iter() {
        let mut ours: Vec<&str> = req.headers.get_all(k).collect();
        let mut theirs: Vec<&str> = f.req_headers.get_all(k).collect();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs, "{name}: header {k}");
    }
    for (k, _) in f.req_headers.iter() {
        if !transport_owned.contains(&k.to_ascii_lowercase().as_str()) {
            assert!(
                req.headers.get(k).is_some(),
                "{name}: capture has header {k} that we do not send"
            );
        }
    }
}

/// Same method, path, headers and a body that is equal as XML.
fn assert_xml_parity(req: &HttpRequest, name: &str) {
    let f = load(name);
    assert_eq!(req.method.as_str(), f.method, "{name}");
    assert_eq!(req.url.path(), f.path, "{name}: path");
    same_headers(req, &f, name);
    let body = req.body_bytes().unwrap();
    assert_eq!(
        canonical_xml(body),
        canonical_xml(&f.req_body),
        "{name}: body"
    );
}

/// Same method, path, headers and byte-identical body.
fn assert_exact_parity(req: &HttpRequest, name: &str) {
    let f = load(name);
    assert_eq!(req.method.as_str(), f.method, "{name}");
    assert_eq!(req.url.path(), f.path, "{name}: path");
    same_headers(req, &f, name);
    let body = req.body_bytes().map(|b| b.to_vec()).unwrap_or_default();
    assert_eq!(
        String::from_utf8_lossy(&body),
        String::from_utf8_lossy(&f.req_body),
        "{name}: body"
    );
}

fn opts(mode: UpdateMode, rev: u64) -> UpdateRequestOptions {
    UpdateRequestOptions {
        target_revision: Some(Revnum(rev)),
        ..UpdateRequestOptions::new(mode, rel("trunk"))
    }
}

fn root(b: &mut UpdateReportBuilder, rev: u64, depth: Depth, start_empty: bool) {
    b.set_path(&RelPath::root(), Revnum(rev), depth, start_empty, None);
}

#[test]
fn checkout_in_both_modes() {
    let u = urls();
    let mut b = UpdateReportBuilder::new(opts(UpdateMode::SendAll, 5));
    root(&mut b, 5, Depth::Infinity, true);
    assert_xml_parity(&b.build(&u).unwrap(), "update/checkout-send-all");

    let mut b = UpdateReportBuilder::new(opts(
        UpdateMode::Skelta {
            include_props: true,
        },
        5,
    ));
    root(&mut b, 5, Depth::Infinity, true);
    assert_xml_parity(&b.build(&u).unwrap(), "update/checkout-skelta");
}

#[test]
fn update_status_and_switch() {
    let u = urls();
    let mut b = UpdateReportBuilder::new(opts(UpdateMode::SendAll, 6));
    root(&mut b, 5, Depth::Infinity, false);
    assert_xml_parity(&b.build(&u).unwrap(), "update/update-send-all");

    let mut b = UpdateReportBuilder::new(opts(
        UpdateMode::Skelta {
            include_props: true,
        },
        11,
    ));
    root(&mut b, 5, Depth::Infinity, false);
    assert_xml_parity(&b.build(&u).unwrap(), "update/update-rich-skelta");

    let status = UpdateRequestOptions {
        text_deltas: false,
        ..UpdateRequestOptions::new(UpdateMode::SendAll, rel("trunk"))
    };
    let mut b = UpdateReportBuilder::new(status);
    root(&mut b, 5, Depth::Infinity, false);
    assert_xml_parity(&b.build(&u).unwrap(), "update/status-u");

    let switch = UpdateRequestOptions {
        dst: Some(rel("trunk")),
        ..opts(UpdateMode::SendAll, 11)
    };
    let mut b = UpdateReportBuilder::new(switch);
    root(&mut b, 11, Depth::Infinity, false);
    assert_xml_parity(&b.build(&u).unwrap(), "update/switch");
}

#[test]
fn sparse_update_of_a_child() {
    let o = UpdateRequestOptions {
        update_target: Some("fresh".into()),
        depth: Depth::Infinity,
        ..opts(UpdateMode::SendAll, 11)
    };
    let mut b = UpdateReportBuilder::new(o);
    root(&mut b, 11, Depth::Empty, false);
    assert_xml_parity(&b.build(&urls()).unwrap(), "update/update-sparse");
}

#[test]
fn switched_and_locked_entries() {
    let token = {
        let f = load("update/update-mixed");
        let body = String::from_utf8(f.req_body).unwrap();
        let start = body.find("opaquelocktoken:").unwrap();
        body[start..].split('"').next().unwrap().to_owned()
    };
    let mut b = UpdateReportBuilder::new(opts(UpdateMode::SendAll, 11));
    root(&mut b, 11, Depth::Infinity, false);
    b.link_path(
        &rel("src"),
        &rel("branches/b1"),
        Revnum(11),
        Depth::Infinity,
        false,
        None,
    );
    b.set_path(
        &rel("a.txt"),
        Revnum(11),
        Depth::Infinity,
        false,
        Some(&token),
    );
    assert_xml_parity(&b.build(&urls()).unwrap(), "update/update-mixed");
}

#[test]
fn report_without_root_entry_is_refused() {
    let b = UpdateReportBuilder::new(opts(UpdateMode::SendAll, 1));
    assert!(b.build(&urls()).is_err());
    let mut b = UpdateReportBuilder::new(opts(UpdateMode::SendAll, 1));
    b.set_path(&rel("a"), Revnum(1), Depth::Infinity, false, None);
    assert!(b.build(&urls()).is_err());
}

fn revprops(f: &Fixture) -> Vec<(String, Vec<u8>)> {
    // The recorded skel lists compat-version, user-agent and the message; reuse them verbatim.
    let body = String::from_utf8_lossy(&f.req_body).into_owned();
    let log_start = body.find("svn:log ").unwrap() + "svn:log ".len();
    let (len, rest) = body[log_start..].split_once(' ').unwrap();
    let len: usize = len.parse().unwrap();
    let ua_start = body.find("svn:txn-user-agent ").unwrap() + "svn:txn-user-agent ".len();
    let (ua_len, ua_rest) = body[ua_start..].split_once(' ').unwrap();
    let ua_len: usize = ua_len.parse().unwrap();
    vec![
        ("svn:txn-client-compat-version".into(), b"1.15.0".to_vec()),
        (
            "svn:txn-user-agent".into(),
            ua_rest.as_bytes()[..ua_len].to_vec(),
        ),
        ("svn:log".into(), rest.as_bytes()[..len].to_vec()),
    ]
}

#[test]
fn create_txn() {
    let f = load("commit/full-create-txn");
    let props = revprops(&f);
    let refs: Vec<(&str, &[u8])> = props
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_slice()))
        .collect();
    let req = create_txn_request(&urls(), &caps(), &refs).unwrap();
    assert_exact_parity(&req, "commit/full-create-txn");
    assert_eq!(
        req.headers.get("Content-Type"),
        Some("application/vnd.svn-skel")
    );
}

#[test]
fn edits_inside_the_transaction() {
    let u = urls();
    let txn = "9-b";
    // MKCOL / DELETE / COPY
    assert_exact_parity(
        &mkcol_request(&u, txn, &rel("trunk/fresh")).unwrap(),
        "commit/full-mkcol",
    );
    assert_exact_parity(
        &delete_request(&u, txn, &rel("trunk/src/b.txt"), Some(Revnum(9)), None).unwrap(),
        "commit/full-delete",
    );
    let copy = copy_request(
        &u,
        Revnum(9),
        &rel("trunk/a.txt"),
        txn,
        &rel("trunk/copy.txt"),
    )
    .unwrap();
    assert_exact_parity(&copy, "commit/full-copy");

    // PROPPATCH: svn:* and custom properties, with and without a base revision.
    let eol = [PropChange {
        name: "svn:eol-style".into(),
        value: Some(Bytes::from_static(b"native")),
    }];
    assert_exact_parity(
        &proppatch_request(&u, txn, &rel("trunk/copy.txt"), &eol, None, None).unwrap(),
        "commit/full-proppatch-copy",
    );
    assert_exact_parity(
        &proppatch_request(&u, txn, &rel("trunk/a.txt"), &eol, Some(Revnum(9)), None).unwrap(),
        "commit/full-proppatch-file",
    );
    let note = [PropChange {
        name: "custom:note".into(),
        value: Some(Bytes::from_static(b"x<y")),
    }];
    assert_exact_parity(
        &proppatch_request(&u, txn, &rel("trunk/src"), &note, Some(Revnum(9)), None).unwrap(),
        "commit/full-proppatch-dir",
    );
}

#[test]
fn put_variants() {
    let u = urls();
    let txn = "9-b";
    let f = load("commit/full-put-new");
    let req = put_request(
        &u,
        txn,
        &rel("trunk/fresh/f.txt"),
        Bytes::from(f.req_body.clone()),
        &PutOptions::default(),
    )
    .unwrap();
    assert_exact_parity(&req, "commit/full-put-new");

    let f = load("commit/full-put-modify");
    let md5 = f
        .req_headers
        .get("X-SVN-Base-Fulltext-MD5")
        .unwrap()
        .to_owned();
    let opts = PutOptions {
        base_rev: Some(Revnum(9)),
        base_md5: Some(&md5),
        ..PutOptions::default()
    };
    let req = put_request(
        &u,
        txn,
        &rel("trunk/a.txt"),
        Bytes::from(f.req_body.clone()),
        &opts,
    )
    .unwrap();
    assert_exact_parity(&req, "commit/full-put-modify");

    let f = load("commit/full-put-empty");
    let req = put_request(
        &u,
        txn,
        &rel("trunk/fresh/empty.txt"),
        Bytes::from(f.req_body.clone()),
        &PutOptions::default(),
    )
    .unwrap();
    assert_exact_parity(&req, "commit/full-put-empty");
}

#[test]
fn put_of_a_locked_file_carries_the_if_header() {
    let f = load("commit/lock-put");
    let if_header = f.req_headers.get("If").unwrap();
    let token = if_header
        .split("(<")
        .nth(1)
        .unwrap()
        .trim_end_matches(">)")
        .to_owned();
    let md5 = f
        .req_headers
        .get("X-SVN-Base-Fulltext-MD5")
        .unwrap()
        .to_owned();
    let txn = f.path.split('/').nth(5).unwrap().to_owned();
    let opts = PutOptions {
        base_rev: Some(Revnum(10)),
        base_md5: Some(&md5),
        lock: Some(LockToken(&token)),
        ..PutOptions::default()
    };
    let req = put_request(
        &urls(),
        &txn,
        &rel("trunk/copy.txt"),
        Bytes::from(f.req_body.clone()),
        &opts,
    )
    .unwrap();
    assert_exact_parity(&req, "commit/lock-put");
}

#[test]
fn merge_and_abort() {
    let u = urls();
    let plain = MergeOptions {
        locks: LockHandling::Release,
        no_merge_response: true,
        lock_tokens: &[],
    };
    assert_exact_parity(
        &merge_request(&u, &rel("trunk"), "9-b", &plain).unwrap(),
        "commit/full-merge",
    );

    let f = load("commit/lock-merge");
    let body = String::from_utf8(f.req_body.clone()).unwrap();
    let token = body
        .split("<lock-token>")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_owned();
    let txn = body
        .split("/txn/")
        .nth(1)
        .unwrap()
        .split('<')
        .next()
        .unwrap()
        .to_owned();
    let locks = [(rel("copy.txt"), token)];
    let with_locks = MergeOptions {
        locks: LockHandling::Release,
        no_merge_response: true,
        lock_tokens: &locks,
    };
    assert_exact_parity(
        &merge_request(&u, &rel("trunk"), &txn, &with_locks).unwrap(),
        "commit/lock-merge",
    );

    let f = load("commit/abort-txn");
    let txn = f.path.rsplit('/').next().unwrap();
    let req = abort_txn_request(&u, txn).unwrap();
    assert_eq!(
        (req.method.as_str(), req.url.path()),
        ("DELETE", f.path.as_str())
    );
}

#[test]
fn blame_request() {
    let req = file_revs_request(
        &urls(),
        Revnum(9),
        &rel("trunk/a.txt"),
        Revnum(0),
        Revnum(9),
        false,
    )
    .unwrap();
    assert_exact_parity(&req, "file_revs/blame");
}
