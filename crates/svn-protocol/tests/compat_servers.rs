//! Replays the builders against the real compat servers (Subversion 1.14.5 and 1.15.0) and feeds
//! the answers to the parsers. Read-only: the shared fixture repositories are never modified.
//!
//! Needs `cargo xtask compat up` and `curl`; run with
//! `cargo test -p svn-protocol --test compat_servers -- --ignored --nocapture --test-threads=1`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod compat_support;

use compat_support::{ok, send};

use svn_protocol::file_revs::{FileRevEvent, file_revs_request, parse_file_revs};
use svn_protocol::list_report::{ListDepth, list_report_request, parse_list_report};
use svn_protocol::locks::{get_locks_request, parse_get_locks};
use svn_protocol::log::{LogRequest, log_request, parse_log};
use svn_protocol::propfind::{ListItem, list_request, parse_list, parse_stat, stat_request};
use svn_protocol::update::{
    DriveChecker, UpdateEvent, UpdateMode, UpdateReportBuilder, UpdateRequestOptions, parse_update,
};
use svn_protocol::{Capabilities, ListStrategy, UrlBuilder, options_request};
use svn_types::{Depth, NodeKind, RelPath, RepoUrl, Revnum};

fn rel(p: &str) -> RelPath {
    RelPath::new(p).unwrap()
}

fn run_against(port: u16, label: &str) {
    let session = RepoUrl::parse(&format!("https://localhost:{port}/svn/basic")).unwrap();
    let (head, body) = send(options_request(&session).unwrap());
    let caps = Capabilities::from_options_response(&session, &head, &body).unwrap();
    println!(
        "{label}: list={} svndiff1/2={}/{} inline-props={} bulk={:?} posts={:?}",
        caps.dav.list,
        caps.dav.svndiff1,
        caps.dav.svndiff2,
        caps.dav.inline_props,
        caps.bulk_updates,
        caps.supported_posts
    );
    let urls = UrlBuilder::new(&session, &caps).unwrap();
    let head_rev = caps.youngest_rev.expect("youngest revision");
    let trunk = rel("trunk");

    // stat + list (PROPFIND) and the list-report, which must agree.
    let (h, b) = send(stat_request(&urls, head_rev, &trunk).unwrap());
    ok(&h, &b, "stat");
    assert_eq!(parse_stat(&urls, &b).unwrap().kind, NodeKind::Dir);
    let (h, b) = send(list_request(&urls, head_rev, &trunk).unwrap());
    ok(&h, &b, "list");
    let (dir, children, rejected) = parse_list(&urls, &trunk, &b).unwrap();
    assert_eq!(dir.name, "trunk");
    assert!(!children.is_empty());
    println!(
        "{label}: trunk has {} entries ({} rejected)",
        children.len(),
        rejected.len()
    );
    if caps.list_strategy() == ListStrategy::ListReport {
        let (h, b) =
            send(list_report_request(&urls, head_rev, &trunk, ListDepth::Immediates).unwrap());
        ok(&h, &b, "list-report");
        let items = parse_list_report(&trunk, &b).unwrap();
        let mut a: Vec<_> = items
            .iter()
            .filter_map(|i| {
                if let ListItem::Entry(e) = i {
                    Some(e.name.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut p: Vec<_> = children.iter().map(|e| e.name.clone()).collect();
        a.sort();
        p.sort();
        assert_eq!(a, p, "list-report and PROPFIND agree");
    }

    // log with changed paths and the locks report.
    let opts = LogRequest {
        start: Some(head_rev),
        end: Some(Revnum(0)),
        discover_changed_paths: true,
        ..LogRequest::default()
    };
    let (h, b) = send(log_request(&urls, head_rev, &trunk, &opts).unwrap());
    ok(&h, &b, "log");
    assert!(!parse_log(&b).unwrap().is_empty());
    let (h, b) = send(get_locks_request(&urls, &trunk, Depth::Infinity).unwrap());
    ok(&h, &b, "get-locks");
    parse_get_locks(&b).unwrap();

    // update-report (checkout) in both modes must be valid editor drives; skelta must fetch
    // every added file and send-all must carry the text itself.
    for (mode, name) in [
        (UpdateMode::SendAll, "send-all"),
        (
            UpdateMode::Skelta {
                include_props: caps.dav.inline_props,
            },
            "skelta",
        ),
    ] {
        let o = UpdateRequestOptions {
            target_revision: Some(head_rev),
            ..UpdateRequestOptions::new(mode, trunk.clone())
        };
        let mut b = UpdateReportBuilder::new(o);
        b.set_path(&RelPath::root(), head_rev, Depth::Infinity, true, None);
        let (h, body) = send(b.build(&urls).unwrap());
        ok(&h, &body, name);
        let events = parse_update(&urls, &body).unwrap();
        let mut checker = DriveChecker::new();
        events.iter().for_each(|e| checker.push(e).unwrap());
        checker.finish().unwrap();
        let count = |f: fn(&UpdateEvent) -> bool| events.iter().filter(|e| f(e)).count();
        let added = count(|e| matches!(e, UpdateEvent::AddFile { .. }));
        let fetched = count(|e| matches!(e, UpdateEvent::FetchFile { .. }));
        let deltas = count(|e| matches!(e, UpdateEvent::TxDeltaStart { .. }));
        println!(
            "{label}: {name}: {} events, {added} files added, {deltas} deltas, {fetched} fetches",
            events.len()
        );
        assert!(added > 0);
        if name == "send-all" {
            assert_eq!(deltas, added);
        } else {
            assert_eq!(fetched, added);
        }
    }

    // blame on the first file of trunk.
    if let Some(file) = children.iter().find(|c| c.kind == NodeKind::File) {
        let path = trunk.child(&file.name).unwrap();
        let (h, b) =
            send(file_revs_request(&urls, head_rev, &path, Revnum(0), head_rev, false).unwrap());
        ok(&h, &b, "file-revs");
        let evs = parse_file_revs(&b).unwrap();
        assert!(evs.iter().any(|e| matches!(e, FileRevEvent::Start { .. })));
        assert!(
            evs.iter()
                .any(|e| matches!(e, FileRevEvent::TxDeltaChunk(_)))
        );
    }
}

#[test]
#[ignore = "needs `cargo xtask compat up` (Docker) and curl"]
fn svn_1_14_5_server() {
    run_against(8443, "1.14.5");
}

#[test]
#[ignore = "needs `cargo xtask compat up` (Docker) and curl"]
fn svn_1_15_0_server() {
    run_against(8444, "1.15.0");
}
