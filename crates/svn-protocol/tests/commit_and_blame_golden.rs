//! MERGE / create-txn answers and file-revs-report against recorded traffic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use bytes::Bytes;
use common::{load, run_split};
use proptest::prelude::*;
use svn_protocol::commit::{MergeResponseParser, parse_create_txn_response, parse_merge_response};
use svn_protocol::file_revs::{FileRevEvent, FileRevsParser, parse_file_revs};
use svn_protocol::{Headers, ResponseHead};
use svn_types::{ErrorCode, Revnum};

#[test]
fn create_txn_answer_names_the_transaction() {
    let f = load("commit/full-create-txn");
    assert_eq!(f.status, 201);
    let name = parse_create_txn_response(&f.head(), &f.resp_body).unwrap();
    assert_eq!(name, f.resp_headers.get("SVN-Txn-Name").unwrap());
}

#[test]
fn create_txn_answer_is_validated() {
    for bad in ["", "..", "a/b", "x y", ".hidden", "a?b"] {
        let head = ResponseHead::new(201, Headers::from_pairs([("SVN-Txn-Name", bad)]));
        assert!(parse_create_txn_response(&head, b"").is_err(), "{bad:?}");
    }
    let head = ResponseHead::new(201, Headers::new());
    assert_eq!(
        parse_create_txn_response(&head, b"").unwrap_err().code,
        ErrorCode::Corrupt
    );
    let virt = ResponseHead::new(201, Headers::from_pairs([("SVN-VTxn-Name", "ab-12")]));
    assert_eq!(parse_create_txn_response(&virt, b"").unwrap(), "ab-12");
}

#[test]
fn merge_answer_gives_the_new_revision() {
    let f = load("commit/full-merge");
    let m = parse_merge_response(&f.head(), &f.resp_body).unwrap();
    assert!(m.new_rev.0 > 0);
    assert_eq!(m.author.as_deref(), Some("alice"));
    assert!(m.date.is_some());
    assert_eq!(m.post_commit_error, None);
    for name in ["commit/merge", "commit/lock-merge"] {
        let f = load(name);
        assert!(
            parse_merge_response(&f.head(), &f.resp_body)
                .unwrap()
                .new_rev
                .0
                > 0,
            "{name}"
        );
    }
}

#[test]
fn a_failing_post_commit_hook_is_reported_but_the_commit_stands() {
    let body = r#"<?xml version="1.0"?><D:merge-response xmlns:D="DAV:" xmlns:S="svn:"><D:updated-set><D:response><D:href>/svn/repo/!svn/vcc/default</D:href><D:propstat><D:prop><D:resourcetype><D:baseline/></D:resourcetype><D:version-name>42</D:version-name><D:creationdate>2026-10-13T08:44:26.336083Z</D:creationdate><D:creator-displayname>bob</D:creator-displayname></D:prop></D:propstat></D:response></D:updated-set><S:post-commit-err>hook said no &lt;1&gt;</S:post-commit-err></D:merge-response>"#;
    let head = ResponseHead::new(200, Headers::new());
    let m = parse_merge_response(&head, body.as_bytes()).unwrap();
    assert_eq!(m.new_rev, Revnum(42));
    assert_eq!(m.post_commit_error.as_deref(), Some("hook said no <1>"));
}

#[test]
fn full_merge_response_picks_the_baseline_not_a_resource() {
    let body = r#"<D:merge-response xmlns:D="DAV:"><D:updated-set>
<D:response><D:href>/svn/repo/!svn/rvr/9/trunk/a.txt</D:href><D:propstat><D:prop><D:resourcetype/><D:version-name>9</D:version-name></D:prop></D:propstat></D:response>
<D:response><D:href>/svn/repo/!svn/vcc/default</D:href><D:propstat><D:prop><D:resourcetype><D:baseline/></D:resourcetype><D:version-name>7</D:version-name></D:prop></D:propstat></D:response>
</D:updated-set></D:merge-response>"#;
    let m = parse_merge_response(&ResponseHead::new(200, Headers::new()), body.as_bytes()).unwrap();
    assert_eq!(m.new_rev, Revnum(7));
}

#[test]
fn merge_failures_use_the_error_chain() {
    let f = load("errors/out-of-date-409");
    let err = parse_merge_response(&f.head(), &f.resp_body).unwrap_err();
    assert_eq!(err.code, ErrorCode::OutOfDate);
    let ok = ResponseHead::new(200, Headers::new());
    assert!(parse_merge_response(&ok, b"<D:merge-response xmlns:D='DAV:'/>").is_err());
    assert!(parse_merge_response(&ok, b"<html/>").is_err());
}

#[test]
fn lock_conflict_on_put_is_reported_as_locked() {
    // A stale lock state makes the server answer 423 with an HTML page.
    let err = svn_protocol::error_from_response(423, &Headers::new(), b"<html>Locked</html>");
    assert_eq!(err.code, ErrorCode::LockedByOther);
}

fn blame_events() -> Vec<FileRevEvent> {
    parse_file_revs(&load("file_revs/blame").resp_body).unwrap()
}

fn joined(events: Vec<FileRevEvent>) -> Vec<FileRevEvent> {
    let mut out: Vec<FileRevEvent> = Vec::new();
    for e in events {
        match (out.last_mut(), e) {
            (Some(FileRevEvent::TxDeltaChunk(prev)), FileRevEvent::TxDeltaChunk(next)) => {
                let mut v = prev.to_vec();
                v.extend_from_slice(&next);
                *prev = Bytes::from(v);
            }
            (_, e) => out.push(e),
        }
    }
    out
}

#[test]
fn blame_report_yields_one_block_per_revision() {
    let evs = joined(blame_events());
    let starts: Vec<(String, u64)> = evs
        .iter()
        .filter_map(|e| match e {
            FileRevEvent::Start { path, rev } => Some((path.as_str().to_owned(), rev.0)),
            _ => None,
        })
        .collect();
    assert!(starts.len() >= 4, "{starts:?}");
    assert!(starts.iter().all(|(p, _)| p == "/trunk/a.txt"));
    assert!(
        starts.windows(2).all(|w| w[0].1 < w[1].1),
        "revisions ascend: {starts:?}"
    );
    let ends = evs
        .iter()
        .filter(|e| matches!(e, FileRevEvent::End))
        .count();
    assert_eq!(ends, starts.len());
}

#[test]
fn blame_report_carries_revprops_props_and_deltas() {
    let evs = joined(blame_events());
    let log2 = evs.iter().find_map(|e| match e {
        FileRevEvent::RevProp { name, value }
            if name == "svn:log" && value.starts_with(b"initial:") =>
        {
            Some(value.clone())
        }
        _ => None,
    });
    assert!(
        log2.unwrap()
            .starts_with(b"initial: files & props <&> unicode")
    );
    assert!(evs.iter().any(|e| matches!(e, FileRevEvent::SetProp { name, value } if name == "custom:tag" && value.as_ref() == br#"v<1>&"q""#)));
    let deltas: Vec<_> = evs
        .iter()
        .filter_map(|e| {
            if let FileRevEvent::TxDeltaChunk(d) = e {
                Some(d)
            } else {
                None
            }
        })
        .collect();
    assert!(deltas.len() >= 4 && deltas.iter().all(|d| d.starts_with(b"SVN")));
    let starts = evs
        .iter()
        .filter(|e| matches!(e, FileRevEvent::TxDeltaStart))
        .count();
    let ends = evs
        .iter()
        .filter(|e| matches!(e, FileRevEvent::TxDeltaEnd))
        .count();
    assert_eq!((starts, ends), (deltas.len(), deltas.len()));
}

#[test]
fn blame_synthetic_merge_and_removed_props() {
    let doc = r#"<S:file-revs-report xmlns:S="svn:">
<S:file-rev path="/branches/x/a.txt" rev="4"><S:merged-revision/><S:rev-prop name="svn:log" encoding="base64">bm90ZQ==</S:rev-prop><S:remove-prop name="old"/><S:txdelta></S:txdelta></S:file-rev></S:file-revs-report>"#;
    let evs = parse_file_revs(doc.as_bytes()).unwrap();
    assert!(evs.contains(&FileRevEvent::MergedRevision));
    assert!(evs.contains(&FileRevEvent::RemoveProp { name: "old".into() }));
    assert!(evs.contains(&FileRevEvent::RevProp {
        name: "svn:log".into(),
        value: Bytes::from_static(b"note")
    }));
    assert!(evs.contains(&FileRevEvent::TxDeltaEnd));
}

#[test]
fn blame_rejects_broken_input() {
    for doc in [
        r#"<S:file-revs-report xmlns:S="svn:"><S:file-rev rev="1"/></S:file-revs-report>"#,
        r#"<S:file-revs-report xmlns:S="svn:"><S:file-rev path="/a" rev="x"/></S:file-revs-report>"#,
        r#"<S:file-revs-report xmlns:S="svn:"><S:file-rev path="/a/../b" rev="1"/></S:file-revs-report>"#,
        r#"<S:file-revs-report xmlns:S="svn:"><S:file-rev path="/a" rev="1"><S:txdelta>%%%%</S:txdelta></S:file-rev></S:file-revs-report>"#,
        r#"<S:file-revs-report xmlns:S="svn:"><S:file-rev path="/a" rev="1"><S:set-prop>v</S:set-prop></S:file-rev></S:file-revs-report>"#,
        "<html/>",
    ] {
        assert!(parse_file_revs(doc.as_bytes()).is_err(), "{doc}");
    }
}

proptest! {
    #[test]
    fn chunking_never_changes_blame_or_merge(cuts in proptest::collection::vec(0usize..16384, 0..16)) {
        let f = load("file_revs/blame");
        prop_assert_eq!(joined(parse_file_revs(&f.resp_body).unwrap()), joined(run_split(FileRevsParser::new(), &f.resp_body, &cuts)));
        let m = load("commit/full-merge");
        let whole = parse_merge_response(&m.head(), &m.resp_body).unwrap();
        let split = run_split(MergeResponseParser::new(), &m.resp_body, &cuts);
        prop_assert_eq!(vec![whole], split);
    }
}
