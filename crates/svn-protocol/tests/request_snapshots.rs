//! Snapshots of every request builder: method, URL, headers and body must stay byte-stable.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{session_url, urls};
use svn_protocol::get::get_request;
use svn_protocol::list_report::{ListDepth, list_report_request};
use svn_protocol::locks::{
    LockOptions, get_lock_request, get_locks_request, lock_request, unlock_request,
};
use svn_protocol::log::{LogRequest, LogRevprops, log_request};
use svn_protocol::propfind::{allprop_request, list_request, stat_request};
use svn_protocol::{HttpRequest, options_request, parent_listing_request};
use svn_types::{Depth, RelPath, RepoUrl, Revnum};

fn render(req: &HttpRequest) -> String {
    let mut out = format!("{} {}\n", req.method.as_str(), req.url);
    for (k, v) in req.headers.iter() {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out.push('\n');
    if let Some(b) = req.body_bytes() {
        out.push_str(&String::from_utf8_lossy(b));
        out.push('\n');
    }
    out
}

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

#[test]
fn options() {
    insta::assert_snapshot!(render(&options_request(&session_url()).unwrap()));
}

#[test]
fn propfind_family() {
    let u = urls();
    insta::assert_snapshot!(
        "stat",
        render(&stat_request(&u, Revnum(12), &rel("trunk/dir with space/\u{e9}.txt")).unwrap())
    );
    insta::assert_snapshot!(
        "list_root",
        render(&list_request(&u, Revnum(0), &RelPath::root()).unwrap())
    );
    insta::assert_snapshot!(
        "allprop",
        render(&allprop_request(&u, Revnum(3), &rel("trunk")).unwrap())
    );
    insta::assert_snapshot!(
        "get_lock",
        render(&get_lock_request(&u, &rel("trunk/a.txt")).unwrap())
    );
}

#[test]
fn get() {
    insta::assert_snapshot!(render(
        &get_request(&urls(), Revnum(7), &rel("trunk/a b.txt")).unwrap()
    ));
}

#[test]
fn log_variants() {
    let u = urls();
    let everything = LogRequest {
        start: Some(Revnum(100)),
        end: Some(Revnum(1)),
        limit: Some(50),
        discover_changed_paths: true,
        strict_node_history: true,
        include_merged_revisions: true,
        revprops: LogRevprops::Only(vec!["svn:author".into(), "custom:x".into()]),
        paths: vec![rel("a"), RelPath::root()],
    };
    insta::assert_snapshot!(
        "log_everything",
        render(&log_request(&u, Revnum(100), &rel("trunk"), &everything).unwrap())
    );
    let minimal = LogRequest {
        revprops: LogRevprops::None,
        ..LogRequest::default()
    };
    insta::assert_snapshot!(
        "log_minimal",
        render(&log_request(&u, Revnum(9), &RelPath::root(), &minimal).unwrap())
    );
    let all = LogRequest {
        revprops: LogRevprops::All,
        ..LogRequest::default()
    };
    insta::assert_snapshot!(
        "log_all_revprops",
        render(&log_request(&u, Revnum(9), &rel("x"), &all).unwrap())
    );
}

#[test]
fn lock_family() {
    let u = urls();
    let p = rel("trunk/a.txt");
    let full = LockOptions {
        comment: Some("a <b> & \"c\"".into()),
        steal: true,
        current_rev: Some(Revnum(4)),
        timeout_secs: Some(600),
    };
    insta::assert_snapshot!("lock_full", render(&lock_request(&u, &p, &full).unwrap()));
    insta::assert_snapshot!(
        "lock_bare",
        render(&lock_request(&u, &p, &LockOptions::default()).unwrap())
    );
    insta::assert_snapshot!(
        "unlock",
        render(&unlock_request(&u, &p, "opaquelocktoken:1234", false).unwrap())
    );
    insta::assert_snapshot!(
        "unlock_break",
        render(&unlock_request(&u, &p, "<opaquelocktoken:1234>", true).unwrap())
    );
    for (name, depth) in [("empty", Depth::Empty), ("infinity", Depth::Infinity)] {
        insta::assert_snapshot!(
            format!("get_locks_{name}"),
            render(&get_locks_request(&u, &rel("trunk"), depth).unwrap())
        );
    }
}

#[test]
fn list_report_family() {
    let u = urls();
    for (name, depth) in [
        ("immediates", ListDepth::Immediates),
        ("infinity", ListDepth::Infinity),
    ] {
        insta::assert_snapshot!(
            format!("list_report_{name}"),
            render(
                &list_report_request(&u, Revnum(5), &rel("trunk/dir with space"), depth).unwrap()
            )
        );
    }
}

#[test]
fn parent_listing() {
    // Without and with the trailing slash: both ask for the same listing page.
    for (name, url) in [
        ("no_slash", "https://svn.test.local/svn"),
        ("slash", "https://svn.test.local/svn/"),
    ] {
        insta::assert_snapshot!(
            format!("parent_listing_{name}"),
            render(&parent_listing_request(&RepoUrl::parse(url).unwrap()).unwrap())
        );
    }
}

#[test]
fn builders_reject_unusable_input() {
    let u = urls();
    let p = rel("a");
    assert!(unlock_request(&u, &p, "", false).is_err());
    assert!(unlock_request(&u, &p, "tok en", false).is_err());
    assert!(unlock_request(&u, &p, "<a><b>", false).is_err());
    assert!(get_locks_request(&u, &p, Depth::Unknown).is_err());
    let bad_comment = LockOptions {
        comment: Some("nul\u{0}".into()),
        ..LockOptions::default()
    };
    assert!(lock_request(&u, &p, &bad_comment).is_err());
}
