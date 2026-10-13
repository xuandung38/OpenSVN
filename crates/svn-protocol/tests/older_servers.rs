//! Servers without the newer capabilities (mod_dav_svn 1.8 - 1.10 era).
//!
//! The OPTIONS answers are synthesized from the recorded 1.14.5 answer by removing the
//! capabilities the older releases did not announce. They stay synthetic until real 1.8, 1.9
//! and 1.10 images can be recorded; behaviour is derived from capabilities only, never from a
//! version string.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{Fixture, load, session_url};
use svn_protocol::commit::{
    PutOptions, create_txn_plan, ephemeral_txn_props, parse_create_txn_response, put_request,
    txn_proppatch_request,
};
use svn_protocol::file_revs::plan_file_revs;
use svn_protocol::list_report::{ListDepth, list_report_request, parse_list_report};
use svn_protocol::propfind::{ListItem, list_request, parse_list};
use svn_protocol::update::{UpdateMode, parse_update};
use svn_protocol::{
    BulkPreference, BulkUpdates, Capabilities, Headers, ListStrategy, ResponseHead, UrlBuilder,
};
use svn_types::{NodeKind, RelPath, Revnum};

const DAV_PREFIX: &str = "http://subversion.tigris.org/xmlns/dav/svn/";

/// The recorded OPTIONS answer without the given capability tokens and headers.
fn options_without(tokens: &[&str], headers: &[&str], posts: &[&str]) -> Fixture {
    let mut f = load("caps/options-200");
    let mut kept = Headers::new();
    for (k, v) in f.resp_headers.iter() {
        let drop_token = k.eq_ignore_ascii_case("DAV")
            && v.strip_prefix(DAV_PREFIX)
                .is_some_and(|t| tokens.contains(&t));
        let drop_header = headers.iter().any(|h| k.eq_ignore_ascii_case(h));
        let drop_post = k.eq_ignore_ascii_case("SVN-Supported-Posts") && posts.contains(&v);
        if !(drop_token || drop_header || drop_post) {
            kept.push(k, v);
        }
    }
    f.resp_headers = kept;
    f
}

fn caps_of(f: &Fixture) -> Capabilities {
    Capabilities::from_options_response(&session_url(), &f.head(), &f.resp_body).unwrap()
}

/// 1.8/1.9 era: no list, no svndiff1/2.
fn caps_1_8_to_1_9() -> Capabilities {
    caps_of(&options_without(
        &["list", "svndiff1", "svndiff2"],
        &[],
        &[],
    ))
}

/// Oldest HTTPv2 shape: none of the later additions at all.
fn caps_minimal_v2() -> Capabilities {
    caps_of(&options_without(
        &[
            "list",
            "svndiff1",
            "svndiff2",
            "inline-props",
            "inherited-props",
            "ephemeral-txnprops",
            "put-result-checksum",
            "reverse-file-revs",
            "replay-rev-resource",
        ],
        &["SVN-Allow-Bulk-Updates", "SVN-Repository-MergeInfo"],
        &["create-txn-with-props"],
    ))
}

fn urls_for(c: &Capabilities) -> UrlBuilder {
    UrlBuilder::new(&session_url(), c).unwrap()
}

#[test]
fn the_current_server_uses_the_newer_paths() {
    let c = caps_of(&load("caps/options-200"));
    assert_eq!(c.list_strategy(), ListStrategy::ListReport);
    assert_eq!(c.svndiff_version(), 2);
    assert!(c.verifies_put_result_checksum());
}

#[test]
fn missing_capabilities_are_read_as_absent_not_as_errors() {
    let c = caps_minimal_v2();
    let d = &c.dav;
    assert!(!(d.list || d.svndiff1 || d.svndiff2 || d.inline_props || d.inherited_props));
    assert!(!(d.ephemeral_txnprops || d.put_result_checksum || d.reverse_file_revs));
    assert_eq!(c.bulk_updates, None);
    assert_eq!(c.repository_mergeinfo, None);
    assert_eq!(c.supported_posts, ["create-txn"]);
    assert!(c.dav.depth && c.dav.mergeinfo && c.dav.log_revprops, "kept");
}

#[test]
fn listing_falls_back_to_propfind() {
    for c in [caps_1_8_to_1_9(), caps_minimal_v2()] {
        assert_eq!(c.list_strategy(), ListStrategy::PropfindDepth1);
    }
    // The fallback request and parser work on the recorded PROPFIND answer.
    let u = urls_for(&caps_1_8_to_1_9());
    let req = list_request(&u, Revnum(5), &RelPath::new("trunk").unwrap()).unwrap();
    assert_eq!(req.headers.get("Depth"), Some("1"));
    let f = load("propfind/list-depth1");
    let (dir, children, rejected) =
        parse_list(&u, &RelPath::new("trunk").unwrap(), &f.resp_body).unwrap();
    assert_eq!(dir.name, "trunk");
    assert_eq!(children.len(), 4);
    assert!(rejected.is_empty());
}

#[test]
fn list_report_and_propfind_agree_on_the_entries() {
    let dir = RelPath::new("trunk").unwrap();
    let report = parse_list_report(&dir, &load("list/list-report").resp_body).unwrap();
    let (pdir, pchildren, _) = parse_list(
        &common::urls(),
        &dir,
        &load("propfind/list-depth1").resp_body,
    )
    .unwrap();
    let mut from_report: Vec<_> = report
        .iter()
        .filter_map(|i| {
            if let ListItem::Entry(e) = i {
                Some((e.name.clone(), format!("{:?}", e.kind), e.size))
            } else {
                None
            }
        })
        .collect();
    let mut from_propfind: Vec<_> = pchildren
        .iter()
        .map(|e| (e.name.clone(), format!("{:?}", e.kind), e.size))
        .collect();
    from_report.sort();
    from_propfind.sort();
    assert_eq!(from_report, from_propfind);
    let ListItem::Dir(rdir) = report
        .iter()
        .find(|i| matches!(i, ListItem::Dir(_)))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        (rdir.name.as_str(), rdir.kind),
        (pdir.name.as_str(), NodeKind::Dir)
    );
}

#[test]
fn list_report_request_matches_the_reference_client() {
    let req = list_report_request(
        &common::urls(),
        Revnum(5),
        &RelPath::new("trunk").unwrap(),
        ListDepth::Immediates,
    )
    .unwrap();
    let f = load("list/list-report");
    assert_eq!(req.url.path(), f.path);
    assert_eq!(
        String::from_utf8_lossy(req.body_bytes().unwrap()),
        String::from_utf8_lossy(&f.req_body)
    );
}

#[test]
fn list_report_rejects_hostile_items() {
    let dir = RelPath::new("trunk").unwrap();
    let wrap = |item: &str| {
        format!(
            r#"<S:list-report xmlns:S="svn:"><S:item node-kind="dir" size="-1" created-rev="1" date="2026-10-13T08:44:26Z">/trunk</S:item>{item}</S:list-report>"#
        )
    };
    let items = parse_list_report(&dir, wrap(r#"<S:item node-kind="file" size="1">/trunk/.svn</S:item><S:item node-kind="file" size="1">/trunk/ok</S:item>"#).as_bytes()).unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, ListItem::Rejected(_)))
            .count(),
        1
    );
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, ListItem::Entry(_)))
            .count(),
        1
    );
    for bad in [
        r#"<S:item node-kind="file">relative</S:item>"#,
        r#"<S:item node-kind="file">/elsewhere/x</S:item>"#,
        r#"<S:item node-kind="file" created-rev="x">/trunk/a</S:item>"#,
    ] {
        assert!(
            parse_list_report(&dir, wrap(bad).as_bytes()).is_err(),
            "{bad}"
        );
    }
    assert!(
        parse_list_report(&dir, b"<S:list-report xmlns:S='svn:'/>").is_err(),
        "own item missing"
    );
}

#[test]
fn update_mode_follows_the_servers_wishes() {
    let current = caps_of(&load("caps/options-200"));
    assert_eq!(current.bulk_updates, Some(BulkUpdates::Prefer));
    assert_eq!(
        UpdateMode::select(&current, BulkPreference::Auto),
        UpdateMode::SendAll
    );
    assert_eq!(
        UpdateMode::select(&current, BulkPreference::Skelta),
        UpdateMode::Skelta {
            include_props: true
        }
    );

    let mut off = current.clone();
    off.bulk_updates = Some(BulkUpdates::Off);
    assert_eq!(
        UpdateMode::select(&off, BulkPreference::Bulk),
        UpdateMode::Skelta {
            include_props: true
        },
        "the administrator disabled bulk updates"
    );

    // No switch at all and no inline properties: bulk is allowed, skelta asks for no props.
    let old = caps_minimal_v2();
    assert!(old.bulk_updates_allowed());
    assert_eq!(
        UpdateMode::select(&old, BulkPreference::Bulk),
        UpdateMode::SendAll
    );
    assert_eq!(
        UpdateMode::select(&old, BulkPreference::Auto),
        UpdateMode::Skelta {
            include_props: false
        }
    );
}

#[test]
fn skelta_answer_without_inline_props_is_parsed() {
    // What a server without `inline-props` sends in skelta mode: no properties, fetch markers.
    let doc = r#"<S:update-report xmlns:S="svn:" xmlns:D="DAV:">
<S:target-revision rev="9"/>
<S:open-directory rev="8"><D:checked-in><D:href>/svn/repo/!svn/rvr/9/trunk</D:href></D:checked-in><S:fetch-props/>
 <S:add-file name="new.txt"><D:checked-in><D:href>/svn/repo/!svn/rvr/9/trunk/new.txt</D:href></D:checked-in><S:fetch-file/><S:fetch-props/></S:add-file>
 <S:open-file name="a.txt" rev="8"><D:checked-in><D:href>/svn/repo/!svn/rvr/9/trunk/a.txt</D:href></D:checked-in><S:fetch-file base-checksum="3e079718d4006e9d757934ce54439418"/></S:open-file>
</S:open-directory></S:update-report>"#;
    let evs = parse_update(&common::urls(), doc.as_bytes()).unwrap();
    use svn_protocol::update::UpdateEvent as E;
    let fetch_files: Vec<_> = evs
        .iter()
        .filter_map(|e| {
            if let E::FetchFile { path, href, .. } = e {
                Some((path.as_str().to_owned(), href.is_some()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        fetch_files,
        [("new.txt".to_owned(), true), ("a.txt".to_owned(), true)]
    );
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, E::FetchProps { .. }))
            .count(),
        2
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, E::SetProp { .. } | E::TxDeltaStart { .. }))
    );
}

#[test]
fn transaction_start_defers_revprops_without_create_txn_with_props() {
    let old = caps_minimal_v2();
    let u = urls_for(&old);
    let plan = create_txn_plan(&u, &old, &[("svn:log", b"message")]).unwrap();
    assert_eq!(plan.request.body_bytes().unwrap(), "(create-txn)");
    assert_eq!(plan.deferred_revprops.len(), 1);
    // After the transaction exists the properties go in a PROPPATCH on the transaction.
    let head = ResponseHead::new(201, Headers::from_pairs([("SVN-Txn-Name", "7-1")]));
    let txn = parse_create_txn_response(&head, b"").unwrap();
    let patch = txn_proppatch_request(&u, &txn, &plan.deferred_revprops).unwrap();
    assert_eq!(patch.url.path(), "/svn/repo/!svn/txn/7-1");
    assert!(
        String::from_utf8_lossy(patch.body_bytes().unwrap()).contains("<S:log>message</S:log>")
    );

    let new = caps_of(&load("caps/options-200"));
    let plan = create_txn_plan(&urls_for(&new), &new, &[("svn:log", b"message")]).unwrap();
    assert!(plan.deferred_revprops.is_empty());
    assert!(
        String::from_utf8_lossy(plan.request.body_bytes().unwrap())
            .starts_with("(create-txn-with-props")
    );
}

#[test]
fn ephemeral_txn_props_need_the_capability() {
    assert!(ephemeral_txn_props(&caps_minimal_v2(), "1.15.0", "ua").is_empty());
    let new = caps_of(&load("caps/options-200"));
    let props = ephemeral_txn_props(&new, "1.15.0", "ua");
    assert_eq!(props.len(), 2);
    assert_eq!(props[0].0, "svn:txn-client-compat-version");
}

#[test]
fn put_result_checksum_and_svndiff_version_follow_capabilities() {
    let md5 = "0123456789abcdef0123456789abcdef";
    let old = caps_minimal_v2();
    assert_eq!(old.svndiff_version(), 0);
    let opts = PutOptions::default().with_result_md5(&old, md5);
    assert_eq!(opts.result_md5, None);
    let req = put_request(
        &urls_for(&old),
        "1-a",
        &RelPath::new("f").unwrap(),
        bytes::Bytes::new(),
        &opts,
    )
    .unwrap();
    assert!(req.headers.get("X-SVN-Result-Fulltext-MD5").is_none());

    let mid = caps_1_8_to_1_9();
    assert_eq!(mid.svndiff_version(), 0);
    assert!(mid.verifies_put_result_checksum());
    let opts = PutOptions::default().with_result_md5(&mid, md5);
    let req = put_request(
        &urls_for(&mid),
        "1-a",
        &RelPath::new("f").unwrap(),
        bytes::Bytes::new(),
        &opts,
    )
    .unwrap();
    assert_eq!(req.headers.get("X-SVN-Result-Fulltext-MD5"), Some(md5));

    let mut only1 = mid.clone();
    only1.dav.svndiff1 = true;
    assert_eq!(only1.svndiff_version(), 1);
}

#[test]
fn descending_file_revs_are_reversed_by_the_client_without_the_capability() {
    let old = caps_minimal_v2();
    let plan = plan_file_revs(&old, Revnum(9), Revnum(2));
    assert_eq!(
        (plan.start, plan.end, plan.client_reverses),
        (Revnum(2), Revnum(9), true)
    );
    let new = caps_of(&load("caps/options-200"));
    let plan = plan_file_revs(&new, Revnum(9), Revnum(2));
    assert_eq!(
        (plan.start, plan.end, plan.client_reverses),
        (Revnum(9), Revnum(2), false)
    );
    let asc = plan_file_revs(&old, Revnum(2), Revnum(9));
    assert!(!asc.client_reverses);
}

#[test]
fn a_server_without_v2_headers_is_still_refused() {
    let f = load("caps/options-200-no-v2-headers");
    assert!(Capabilities::from_options_response(&session_url(), &f.head(), &f.resp_body).is_err());
}
