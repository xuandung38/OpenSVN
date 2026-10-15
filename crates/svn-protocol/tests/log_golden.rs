//! log-report parsing against recorded traffic, cross-checked with `svn log --xml` output.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{
    DATA_SETS, SvnXmlLogRow, companion, for_each_set, load, run_split, svn_log_xml, use_set,
};
use proptest::prelude::*;
use svn_protocol::log::{LogItem, LogParser, parse_log};
use svn_protocol::{ResponseParser, parse_timestamp};
use svn_types::{ChangeAction, ErrorCode, NodeKind, Revnum};

fn action_letter(a: ChangeAction) -> &'static str {
    match a {
        ChangeAction::Added => "A",
        ChangeAction::Modified => "M",
        ChangeAction::Deleted => "D",
        ChangeAction::Replaced => "R",
    }
}

fn kind_word(k: NodeKind) -> &'static str {
    match k {
        NodeKind::File => "file",
        NodeKind::Dir => "dir",
        _ => "unknown",
    }
}

/// The parsed items must carry exactly what the reference client printed.
fn assert_matches_svn_cli(name: &str) {
    let f = load(name);
    assert_eq!(f.method, "REPORT");
    assert!(
        String::from_utf8_lossy(&f.req_body).contains("log-report"),
        "{name} is not a log request"
    );
    // The CLI hides r0 when it has no message; the wire protocol still sends it.
    let items: Vec<LogItem> = parse_log(&f.resp_body)
        .unwrap()
        .into_iter()
        .filter(|i| !(i.entry.rev.0 == 0 && i.entry.message.is_none()))
        .collect();
    let rows: Vec<SvnXmlLogRow> = svn_log_xml(&companion(name, "svn.xml"));
    assert_eq!(items.len(), rows.len(), "{name}: entry count");
    for (item, row) in items.iter().zip(&rows) {
        let e = &item.entry;
        assert_eq!(e.rev, Revnum(row.rev), "{name}");
        assert_eq!(item.merge_depth, row.depth, "{name} r{}", row.rev);
        assert_eq!(e.author, row.author, "{name} r{}", row.rev);
        assert_eq!(
            e.date,
            row.date.as_deref().map(|d| parse_timestamp(d).unwrap()),
            "{name} r{}",
            row.rev
        );
        assert_eq!(
            e.message.clone().unwrap_or_default(),
            row.msg.clone().unwrap_or_default(),
            "{name} r{}",
            row.rev
        );
        let mut ours: Vec<_> = item
            .entry
            .changed_paths
            .iter()
            .map(|p| {
                (
                    action_letter(p.action).to_owned(),
                    kind_word(p.kind).to_owned(),
                    p.path.as_str().to_owned(),
                    p.copy_from.as_ref().map(|c| c.0.as_str().to_owned()),
                    p.copy_from.as_ref().map(|c| c.1.0.to_string()),
                )
            })
            .collect();
        let mut theirs = row.paths.clone();
        ours.sort();
        theirs.sort();
        assert_eq!(ours, theirs, "{name} r{} changed paths", row.rev);
    }
}

#[test]
fn verbose_log_matches_svn_cli() {
    assert_matches_svn_cli("log/head-verbose");
}

#[test]
fn log_with_copy_matches_svn_cli() {
    assert_matches_svn_cli("log/root-with-copy");
}

#[test]
fn limited_log_with_all_revprops_matches_svn_cli() {
    for_each_set(|| {
        assert_matches_svn_cli("log/limit2-all-revprops");
        assert_eq!(
            parse_log(&load("log/limit2-all-revprops").resp_body)
                .unwrap()
                .len(),
            2
        );
    });
}

#[test]
fn merged_revisions_are_nested_and_the_end_marker_is_consumed() {
    for_each_set(|| {
        assert_matches_svn_cli("log/merged-revisions");
        let items = parse_log(&load("log/merged-revisions").resp_body).unwrap();
        let shape: Vec<(u64, u32, bool)> = items
            .iter()
            .map(|i| (i.entry.rev.0, i.merge_depth, i.entry.has_children))
            .collect();
        assert_eq!(
            shape.iter().map(|s| s.1).collect::<Vec<_>>(),
            [0, 1, 1, 0, 0]
        );
        assert!(shape[0].2, "the merge commit has children");
        assert!(
            items.iter().all(|i| i.entry.rev.0 != u64::MAX),
            "the -1 marker must not leak"
        );
    });
}

#[test]
fn copy_source_is_reported() {
    for_each_set(|| {
        let items = parse_log(&load("log/head5-verbose").resp_body).unwrap();
        assert!(!items.is_empty());
        let all = parse_log(&load("log/root-with-copy").resp_body).unwrap();
        let copied = all
            .iter()
            .flat_map(|i| &i.entry.changed_paths)
            .find(|p| p.copy_from.is_some())
            .expect("the capture contains a copy");
        assert_eq!(copied.action, ChangeAction::Added);
        assert_eq!(copied.path.as_str(), "/branches/b1");
        let (src, rev) = copied.copy_from.as_ref().unwrap();
        assert_eq!((src.as_str(), *rev), ("/trunk/src", Revnum(4)));
    });
}

#[test]
fn entry_without_changed_paths_or_message() {
    for_each_set(|| {
        let items = parse_log(&load("log/single-rev-no-paths").resp_body).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].entry.changed_paths.is_empty());
        assert_eq!(items[0].entry.message, None);
        assert_eq!(items[0].entry.rev, Revnum(2));
    });
}

#[test]
fn unicode_and_markup_in_messages_survive() {
    for_each_set(|| {
        let items = parse_log(&load("log/head-verbose").resp_body).unwrap();
        let initial = items.iter().find(|i| i.entry.rev == Revnum(2)).unwrap();
        let msg = initial.entry.message.as_deref().unwrap();
        assert!(
            msg.starts_with("initial: files & props <&> unicode"),
            "{msg}"
        );
    });
}

#[test]
fn items_stream_out_before_the_document_ends() {
    for_each_set(|| {
        let f = load("log/head-verbose");
        let mut p = LogParser::new();
        let cut = f.resp_body.len() * 2 / 3;
        let early = p.feed(&f.resp_body[..cut]).unwrap();
        assert!(
            !early.is_empty(),
            "no item emitted after two thirds of the body"
        );
        let mut all = early;
        all.extend(p.feed(&f.resp_body[cut..]).unwrap());
        all.extend(p.finish().unwrap());
        assert_eq!(all, parse_log(&f.resp_body).unwrap());
    });
}

#[test]
fn hostile_or_broken_documents_are_errors() {
    let wrap = |inner: &str| {
        format!(
            r#"<S:log-report xmlns:S="svn:" xmlns:D="DAV:"><S:log-item>{inner}</S:log-item></S:log-report>"#
        )
    };
    let bad = [
        wrap(
            r#"<S:added-path node-kind="file">/trunk/../../etc/passwd</S:added-path><D:version-name>1</D:version-name>"#,
        ),
        wrap(
            r#"<S:added-path node-kind="file">relative</S:added-path><D:version-name>1</D:version-name>"#,
        ),
        wrap(
            r#"<S:added-path node-kind="file" copyfrom-path="/a/../b" copyfrom-rev="1">/x</S:added-path><D:version-name>1</D:version-name>"#,
        ),
        wrap("<D:version-name>abc</D:version-name>"),
        wrap("<S:date>nonsense</S:date><D:version-name>1</D:version-name>"),
        wrap("<S:date>2026-10-13T08:44:26Z</S:date>"),
        "<html></html>".to_owned(),
        "<S:log-report xmlns:S='svn:'><S:log-item>".to_owned(),
    ];
    for doc in bad {
        assert!(parse_log(doc.as_bytes()).is_err(), "{doc}");
    }
    // Names awkward on Windows are fine in a display-only log.
    let ok = wrap(
        r#"<S:added-path node-kind="file">/trunk/aux.c</S:added-path><D:version-name>1</D:version-name>"#,
    );
    assert_eq!(
        parse_log(ok.as_bytes()).unwrap()[0].entry.changed_paths[0]
            .path
            .as_str(),
        "/trunk/aux.c"
    );
}

#[test]
fn unknown_elements_are_ignored() {
    let doc = r#"<S:log-report xmlns:S="svn:" xmlns:D="DAV:" xmlns:X="urn:future">
<S:log-item><X:new-thing><X:deep/></X:new-thing><D:version-name>7</D:version-name><S:revprop name="x">y</S:revprop><S:subtractive-merge/></S:log-item></S:log-report>"#;
    let items: Vec<LogItem> = parse_log(doc.as_bytes()).unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].subtractive_merge);
}

#[test]
fn http_errors_are_not_log_documents() {
    for_each_set(|| {
        let f = load("errors/not-found-404-html");
        assert_eq!(
            parse_log(&f.resp_body).unwrap_err().code,
            ErrorCode::Corrupt
        );
    });
}

proptest! {
    #[test]
    fn chunking_never_changes_the_result(cuts in proptest::collection::vec(0usize..16384, 0..16)) {
        for set in DATA_SETS {
            use_set(set);
            for name in ["log/head-verbose", "log/merged-revisions", "log/root-with-copy"] {
                let f = load(name);
                let whole = parse_log(&f.resp_body).unwrap();
                let split = run_split(LogParser::new(), &f.resp_body, &cuts);
                prop_assert_eq!(whole, split);
            }
        }
    }

    #[test]
    fn arbitrary_garbage_never_panics(data in proptest::collection::vec(any::<u8>(), 0..400)) {
        let _ = parse_log(&data);
    }
}

#[test]
fn display_paths_are_lenient_but_stay_out_of_the_contract_until_asked() {
    for_each_set(|| {
        use svn_protocol::RepoDisplayPath;
        let items = parse_log(&load("log/head-verbose").resp_body).unwrap();
        let item = items
            .into_iter()
            .find(|i| i.entry.rev == Revnum(2))
            .unwrap();
        let entry = item.entry;
        assert!(!entry.changed_paths.is_empty());
        assert!(
            entry
                .changed_paths
                .iter()
                .all(|p| p.path.as_str().starts_with('/'))
        );
        // Windows-reserved and colon names display fine, and need an explicit step to become a RelPath.
        let p = RepoDisplayPath::parse("/trunk/aux.c:stream").unwrap();
        assert!(svn_types::RelPath::from_server(p.as_str().trim_start_matches('/')).is_err());
        assert!(RepoDisplayPath::parse("/a/../b").is_err());
    });
}
