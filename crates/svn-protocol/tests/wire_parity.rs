//! Builders must produce what the reference client (svn 1.15 / serf) put on the wire.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fixture, load, session_url, urls};
use svn_protocol::get::get_request;
use svn_protocol::locks::{
    LockOptions, get_lock_request, get_locks_request, lock_request, unlock_request,
};
use svn_protocol::log::{LogRequest, LogRevprops, log_request};
use svn_protocol::propfind::{allprop_request, list_request, stat_request};
use svn_protocol::{HttpRequest, options_request};
use svn_types::{Depth, RelPath, Revnum};

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

/// Method, path, body and every header we send must match the capture.
fn assert_wire_equal(req: &HttpRequest, name: &str) {
    let f: Fixture = load(name);
    assert_eq!(req.method.as_str(), f.method, "{name}: method");
    assert_eq!(req.url.path(), f.path, "{name}: path");
    assert_eq!(req.url.host_str(), Some("svn.test.local"));
    let body = req.body_bytes().map(|b| b.to_vec()).unwrap_or_default();
    assert_eq!(
        String::from_utf8_lossy(&body),
        String::from_utf8_lossy(&f.req_body),
        "{name}: body"
    );
    for (k, _) in req.headers.iter() {
        let mut ours: Vec<&str> = req.headers.get_all(k).collect();
        let mut theirs: Vec<&str> = f.req_headers.get_all(k).collect();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs, "{name}: header {k}");
    }
    // Headers the reference client sends and we deliberately leave to the transport.
    let transport_owned = [
        "user-agent",
        "accept",
        "host",
        "authorization",
        "content-length",
        "transfer-encoding",
    ];
    for (k, _) in f.req_headers.iter() {
        if transport_owned.contains(&k.to_ascii_lowercase().as_str()) {
            continue;
        }
        assert!(
            req.headers.get(k).is_some(),
            "{name}: capture has header {k} that we do not send"
        );
    }
}

#[test]
fn options() {
    assert_wire_equal(
        &options_request(&session_url()).unwrap(),
        "caps/options-200",
    );
}

#[test]
fn propfind_stat_list_allprop_and_get_lock() {
    let u = urls();
    assert_wire_equal(
        &stat_request(&u, Revnum(5), &rel("trunk/src")).unwrap(),
        "propfind/stat-dir",
    );
    assert_wire_equal(
        &list_request(&u, Revnum(5), &rel("trunk")).unwrap(),
        "propfind/list-depth1",
    );
    assert_wire_equal(
        &allprop_request(&u, Revnum(5), &rel("trunk/a.txt")).unwrap(),
        "propfind/allprop-file",
    );
    assert_wire_equal(
        &get_lock_request(&u, &rel("trunk/a.txt")).unwrap(),
        "propfind/get-lock",
    );
}

#[test]
fn get() {
    let r = get_request(&urls(), Revnum(5), &rel("trunk/a.txt")).unwrap();
    let f = load("get/file");
    assert_eq!(
        (r.method.as_str(), r.url.path()),
        (f.method.as_str(), f.path.as_str())
    );
    assert!(r.body_bytes().is_none());
}

#[test]
fn lock_unlock_and_get_locks() {
    let u = urls();
    let p = rel("trunk/a.txt");
    let plain = LockOptions {
        comment: Some("locking <it>".into()),
        ..LockOptions::default()
    };
    assert_wire_equal(&lock_request(&u, &p, &plain).unwrap(), "locks/lock-ok");
    let steal = LockOptions {
        comment: Some("stolen".into()),
        steal: true,
        ..LockOptions::default()
    };
    assert_wire_equal(&lock_request(&u, &p, &steal).unwrap(), "locks/lock-steal");
    let from_wc = LockOptions {
        comment: Some("from wc".into()),
        current_rev: Some(Revnum(9)),
        ..LockOptions::default()
    };
    assert_wire_equal(
        &lock_request(&u, &p, &from_wc).unwrap(),
        "locks/lock-from-wc",
    );

    let f = load("locks/unlock-204");
    let token = f.req_headers.get("Lock-Token").unwrap().to_owned();
    let unlock = unlock_request(&u, &rel("trunk/src/b.txt"), &token, false).unwrap();
    assert_wire_equal(&unlock, "locks/unlock-204");
    // The token is accepted with or without brackets.
    let bare = token.trim_matches(['<', '>']).to_owned();
    let again = unlock_request(&u, &rel("trunk/src/b.txt"), &bare, false).unwrap();
    assert_eq!(again.headers.get("Lock-Token"), Some(token.as_str()));

    let f = load("locks/unlock-break");
    let token = f.req_headers.get("Lock-Token").unwrap().to_owned();
    assert_wire_equal(
        &unlock_request(&u, &p, &token, true).unwrap(),
        "locks/unlock-break",
    );

    assert_wire_equal(
        &get_locks_request(&u, &rel("trunk"), Depth::Immediates).unwrap(),
        "locks/get-locks-report",
    );
}

#[test]
fn log_reports() {
    let u = urls();
    let head5 = LogRequest {
        start: Some(Revnum(5)),
        end: Some(Revnum(0)),
        discover_changed_paths: true,
        ..LogRequest::default()
    };
    assert_wire_equal(
        &log_request(&u, Revnum(5), &rel("trunk"), &head5).unwrap(),
        "log/head5-verbose",
    );

    let limited = LogRequest {
        start: Some(Revnum(5)),
        end: Some(Revnum(0)),
        limit: Some(2),
        discover_changed_paths: true,
        revprops: LogRevprops::All,
        ..LogRequest::default()
    };
    assert_wire_equal(
        &log_request(&u, Revnum(5), &RelPath::root(), &limited).unwrap(),
        "log/limit2-all-revprops",
    );

    let merged = LogRequest {
        start: Some(Revnum(8)),
        end: Some(Revnum(0)),
        limit: Some(3),
        discover_changed_paths: true,
        include_merged_revisions: true,
        ..LogRequest::default()
    };
    assert_wire_equal(
        &log_request(&u, Revnum(8), &rel("trunk"), &merged).unwrap(),
        "log/merged-revisions",
    );

    let quiet = LogRequest {
        start: Some(Revnum(2)),
        end: Some(Revnum(2)),
        revprops: LogRevprops::Only(vec!["svn:author".into(), "svn:date".into()]),
        ..LogRequest::default()
    };
    assert_wire_equal(
        &log_request(&u, Revnum(2), &RelPath::root(), &quiet).unwrap(),
        "log/single-rev-no-paths",
    );
}
