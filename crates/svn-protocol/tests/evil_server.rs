//! Hostile-server fixtures (`fixtures/evil/`, see its README): every document a malicious or
//! broken server could send must be rejected with a typed error, never accepted, never panic.
//! Each XML case is also fed one byte at a time, so the incremental parsers cannot be talked
//! into a different verdict by chunking.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;

use common::{session_url, urls};
use svn_protocol::commit::{MergeResponseParser, parse_merge_response};
use svn_protocol::locks::{GetLocksParser, parse_get_locks};
use svn_protocol::log::{LogParser, parse_log};
use svn_protocol::propfind::{ListItem, ListParser, parse_list};
use svn_protocol::update::{UpdateParser, parse_update};
use svn_protocol::{Capabilities, Headers, ResponseHead, ResponseParser, UrlBuilder};
use svn_types::{ErrorCode, RelPath, Result};

struct Case {
    id: String,
    parser: String,
    body: Vec<u8>,
    expect: String,
}

fn evil_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/evil")
}

fn cases(parsers: &[&str]) -> Vec<Case> {
    let index = std::fs::read_to_string(evil_dir().join("cases.tsv")).expect("cases.tsv");
    let out: Vec<Case> = index
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let cols: Vec<&str> = l.split('\t').collect();
            assert!(cols.len() >= 4, "bad cases.tsv row: {l}");
            Case {
                id: cols[0].to_owned(),
                parser: cols[1].to_owned(),
                body: std::fs::read(evil_dir().join(cols[2]))
                    .unwrap_or_else(|e| panic!("{}: {e}", cols[2])),
                expect: cols[3].to_owned(),
            }
        })
        .filter(|c| parsers.contains(&c.parser.as_str()))
        .collect();
    assert!(!out.is_empty(), "no cases for {parsers:?}");
    out
}

/// Feeds `body` one byte at a time; the first error wins.
fn bytewise<P: ResponseParser>(mut p: P, body: &[u8]) -> Result<Vec<P::Item>> {
    let mut items = Vec::new();
    for b in body.chunks(1) {
        items.extend(p.feed(b)?);
    }
    items.extend(p.finish()?);
    Ok(items)
}

/// Asserts `whole` and `split` both fail with a protocol-safety error. `unsafe` cases may
/// surface as `UnsafePath` or as `Corrupt` (a name no server could legitimately send); either way
/// nothing reaches the caller. Returns the failures instead of stopping at the first one.
fn check_rejected<T: std::fmt::Debug>(
    case: &Case,
    whole: Result<T>,
    split: Result<T>,
) -> Vec<String> {
    let mut bad = Vec::new();
    for (how, r) in [("whole", whole), ("bytewise", split)] {
        match r {
            Ok(v) => bad.push(format!(
                "{} ({how}): hostile document was ACCEPTED: {v:?}",
                case.id
            )),
            Err(e) if matches!(e.code, ErrorCode::UnsafePath | ErrorCode::Corrupt) => {}
            Err(e) => bad.push(format!("{} ({how}): unexpected error kind {e:?}", case.id)),
        }
    }
    bad
}

fn assert_all(failures: Vec<String>) {
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn update_reports_with_traversal_or_garbage_are_rejected() {
    let mut failures = Vec::new();
    let u = urls();
    for c in cases(&["update"]) {
        let whole = parse_update(&u, &c.body);
        let split = bytewise(UpdateParser::new(u.clone()), &c.body);
        failures.extend(check_rejected(&c, whole, split));
    }
    assert_all(failures);
}

#[test]
fn propfind_listings_with_foreign_or_traversing_hrefs_are_rejected() {
    let u = urls();
    let target = RelPath::new("trunk").unwrap();
    for c in cases(&["propfind-list"]) {
        assert_eq!(c.expect, "quarantine", "{}", c.id);
        match parse_list(&u, &target, &c.body) {
            Ok((_, children, rejected)) => {
                assert!(
                    children.is_empty(),
                    "{}: hostile child listed: {children:?}",
                    c.id
                );
                assert_eq!(rejected.len(), 1, "{}", c.id);
                let code = rejected[0].error.code;
                assert!(
                    matches!(code, ErrorCode::UnsafePath | ErrorCode::Corrupt),
                    "{}: {code:?}",
                    c.id
                );
            }
            Err(e) => assert!(
                matches!(e.code, ErrorCode::UnsafePath | ErrorCode::Corrupt),
                "{}: {e:?}",
                c.id
            ),
        }
        // Byte-at-a-time feeding never lets the child through either.
        if let Ok(items) = bytewise(ListParser::new(&u, target.clone()), &c.body) {
            assert!(
                !items.iter().any(|i| matches!(i, ListItem::Entry(_))),
                "{}: hostile child listed when fed bytewise",
                c.id
            );
        }
    }
}

#[test]
fn log_reports_reject_bad_paths_and_keep_html_as_text() {
    let mut failures = Vec::new();
    for c in cases(&["log"]) {
        let whole = parse_log(&c.body);
        let split = bytewise(LogParser::new(), &c.body);
        if c.expect == "ok-verbatim" {
            let items = whole.unwrap_or_else(|e| panic!("{}: {e:?}", c.id));
            assert_eq!(items, split.unwrap(), "{}", c.id);
            let msg = items[0].entry.message.clone().unwrap_or_default();
            // Delivered verbatim as text; escaping is the renderer's job, never the parser's.
            assert!(msg.contains("<script>"), "{}: {msg}", c.id);
            continue;
        }
        failures.extend(check_rejected(&c, whole, split));
    }
    assert_all(failures);
}

#[test]
fn merge_responses_with_bogus_revisions_are_rejected() {
    let mut failures = Vec::new();
    let head = ResponseHead::new(200, Headers::new());
    for c in cases(&["merge"]) {
        let whole = parse_merge_response(&head, &c.body).map(|m| vec![m]);
        let split = bytewise(MergeResponseParser::new(), &c.body);
        failures.extend(check_rejected(&c, whole, split));
    }
    assert_all(failures);
}

#[test]
fn lock_reports_with_bad_paths_are_rejected() {
    let mut failures = Vec::new();
    for c in cases(&["get-locks"]) {
        let whole = parse_get_locks(&c.body);
        let split = bytewise(GetLocksParser::new(), &c.body);
        failures.extend(check_rejected(&c, whole, split));
    }
    assert_all(failures);
}

/// OPTIONS headers that would move later requests to another origin or outside the repository.
#[test]
fn options_answers_pointing_elsewhere_are_rejected() {
    let session = session_url();
    for c in cases(&["options"]) {
        assert!(c.expect == "reject", "{}", c.id);
        let text = String::from_utf8(c.body.clone()).unwrap();
        let mut headers = Headers::new();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let (k, v) = line.split_once(": ").unwrap();
            headers.push(k, v);
        }
        let head = ResponseHead::new(200, headers);
        let verdict = Capabilities::from_options_response(&session, &head, b"")
            .and_then(|caps| UrlBuilder::new(&session, &caps));
        match verdict {
            Err(e) => assert!(
                matches!(
                    e.code,
                    ErrorCode::Corrupt | ErrorCode::UnsafePath | ErrorCode::ServerUnsupported
                ),
                "{}: unexpected error kind {e:?}",
                c.id
            ),
            Ok(_) => panic!("{}: hostile OPTIONS answer was accepted", c.id),
        }
    }
}

/// The fixture index and the generator stay in step (no orphaned or missing files).
#[test]
fn evil_fixture_index_is_complete() {
    let index = std::fs::read_to_string(evil_dir().join("cases.tsv")).unwrap();
    let mut listed = 0;
    for line in index
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let file = line.split('\t').nth(2).unwrap();
        assert!(evil_dir().join(file).is_file(), "missing {file}");
        listed += 1;
    }
    assert!(listed >= 60, "only {listed} cases listed");
}
