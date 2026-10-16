//! The update parser is the gate for the editor drive order: every violation is `ProtocolViolation`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{load, urls};
use svn_protocol::update::{DriveChecker, UpdateEvent, parse_update};
use svn_types::ErrorCode;

const NS: &str = r#"xmlns:S="svn:" xmlns:D="DAV:" xmlns:X="urn:x" xmlns:V="http://subversion.tigris.org/xmlns/dav/""#;

fn report(attrs: &str, inner: &str) -> String {
    format!("<S:update-report {NS} {attrs}>{inner}</S:update-report>")
}

fn root(inner: &str) -> String {
    report(
        "",
        &format!(
            r#"<S:target-revision rev="2"/><S:open-directory rev="1">{inner}</S:open-directory>"#
        ),
    )
}

fn parse(doc: &str) -> svn_types::Result<Vec<UpdateEvent>> {
    parse_update(&urls(), doc.as_bytes())
}

fn check(evs: &[UpdateEvent]) {
    let mut c = DriveChecker::new();
    for e in evs {
        c.push(e).unwrap();
    }
    c.finish().unwrap();
}

fn corrupt(doc: &str, why: &str) {
    let err = parse(doc).expect_err(why);
    assert_eq!(err.code, ErrorCode::ProtocolViolation, "{why}: {err}");
}

#[test]
fn foreign_namespace_end_tags_never_pop_nodes() {
    // The element closes itself; the root must stay open until its own end tag.
    let evs = parse(&root(
        r#"<X:add-file name="y"></X:add-file><S:add-file name="f"/>"#,
    ))
    .unwrap();
    let close_root = evs
        .iter()
        .position(|e| matches!(e, UpdateEvent::CloseDir { path } if path.is_root()))
        .unwrap();
    assert!(
        evs[..close_root]
            .iter()
            .any(|e| matches!(e, UpdateEvent::AddFile { .. }))
    );
    assert_eq!(close_root, evs.len() - 1, "root closes last");
    check(&evs);
    // A foreign element wrapping a real entry is not a parent for it.
    corrupt(
        &root(r#"<X:wrap><S:add-file name="f"/></X:wrap>"#),
        "entry inside a foreign wrapper",
    );
    // The same local name in another namespace is not an entry.
    let evs = parse(&root(
        r#"<X:open-file name="zzz"><S:set-prop name="p">v</S:set-prop></X:open-file>"#,
    ));
    assert!(evs.is_err(), "property change under a foreign element");
}

#[test]
fn at_most_one_text_delta_per_file() {
    corrupt(
        &root(
            r#"<S:add-file name="f"><S:txdelta>U1ZOAA==</S:txdelta><S:txdelta>U1ZOAA==</S:txdelta></S:add-file>"#,
        ),
        "two deltas",
    );
    corrupt(
        &root(
            r#"<S:add-file name="f"><S:txdelta>U1ZOAA==</S:txdelta><S:fetch-file/></S:add-file>"#,
        ),
        "delta then fetch",
    );
    corrupt(
        &root(r#"<S:add-file name="f"><S:fetch-file/><S:fetch-file/></S:add-file>"#),
        "two fetches",
    );
    // A delete followed by an add of the same name is a different file.
    let evs = parse(&root(r#"<S:delete-entry name="f"/><S:add-file name="f"><S:txdelta>U1ZOAA==</S:txdelta></S:add-file>"#)).unwrap();
    check(&evs);
}

#[test]
fn no_text_delta_on_a_directory() {
    corrupt(
        &root(r#"<S:add-directory name="d"><S:txdelta>U1ZOAA==</S:txdelta></S:add-directory>"#),
        "delta in directory",
    );
    corrupt(
        &report("", r#"<S:txdelta>U1ZOAA==</S:txdelta>"#),
        "delta before any node",
    );
    corrupt(
        &root(r#"<S:txdelta>U1ZOAA==</S:txdelta>"#),
        "delta in the root",
    );
    corrupt(&root(r#"<S:fetch-file/>"#), "fetch-file in the root");
}

#[test]
fn files_have_no_children() {
    corrupt(
        &root(r#"<S:add-file name="f"><S:add-file name="g"/></S:add-file>"#),
        "file in file",
    );
    corrupt(
        &root(r#"<S:open-file name="f" rev="1"><S:delete-entry name="x"/></S:open-file>"#),
        "delete in file",
    );
    corrupt(
        &root(r#"<S:open-file name="f" rev="1"><S:add-directory name="d"/></S:open-file>"#),
        "dir in file",
    );
    corrupt(
        &root(r#"<S:open-file name="f" rev="1"><S:absent-file name="a"/></S:open-file>"#),
        "absent in file",
    );
}

#[test]
fn exactly_one_root_and_nothing_after_it() {
    corrupt(
        &report(
            "",
            r#"<S:open-directory rev="1"/><S:open-directory rev="1"/>"#,
        ),
        "two roots",
    );
    corrupt(
        &report(
            "",
            r#"<S:open-directory rev="1"></S:open-directory><S:add-file name="late"/>"#,
        ),
        "entry after root",
    );
    corrupt(
        &report(
            "",
            r#"<S:open-directory rev="1"></S:open-directory><S:delete-entry name="late"/>"#,
        ),
        "delete after root",
    );
    corrupt(
        &report(
            "",
            r#"<S:open-directory rev="1"></S:open-directory><S:target-revision rev="3"/>"#,
        ),
        "target after root",
    );
    corrupt(
        &report("", r#"<S:add-directory/>"#),
        "root must be open-directory",
    );
    corrupt(
        &report("", r#"<S:open-directory name="x" rev="1"/>"#),
        "root must be anonymous",
    );
    corrupt(&report("", r#"<S:add-file name="f"/>"#), "file as root");
    corrupt(
        &root(r#"<S:target-revision rev="3"/>"#),
        "target inside the tree",
    );
    corrupt(
        &report(
            "",
            r#"<S:target-revision rev="1"/><S:target-revision rev="2"/><S:open-directory rev="1"/>"#,
        ),
        "two targets",
    );
    // An empty answer (nothing to do) is fine.
    assert!(parse(&report("", r#"<S:target-revision rev="2"/>"#)).is_ok());
}

#[test]
fn mismatched_or_stray_end_tags_are_errors() {
    assert!(parse("<S:update-report xmlns:S='svn:'></S:update-report></S:update-report>").is_err());
    assert!(parse("<S:update-report xmlns:S='svn:' xmlns:Y='svn:'></Y:update-report>").is_err());
    corrupt(&root(r#"<S:add-file name="f">"#), "truncated");
}

#[test]
fn unknown_property_encoding_is_an_error() {
    corrupt(
        &root(r#"<S:set-prop name="p" encoding="rot13">abc</S:set-prop>"#),
        "set-prop encoding",
    );
    let ok = parse(&root(
        r#"<S:set-prop name="p" encoding="base64">dmFs</S:set-prop>"#,
    ))
    .unwrap();
    assert!(
        ok.iter()
            .any(|e| matches!(e, UpdateEvent::SetProp { value, .. } if value.as_ref() == b"val"))
    );
}

#[test]
fn report_mode_is_reported_first() {
    let evs = parse(&report(r#"send-all="true" inline-props="true""#, "")).unwrap();
    assert_eq!(
        evs[0],
        UpdateEvent::ReportStart {
            send_all: true,
            inline_props: true
        }
    );
    let evs = parse(&report("", "")).unwrap();
    assert_eq!(
        evs[0],
        UpdateEvent::ReportStart {
            send_all: false,
            inline_props: false
        }
    );
}

#[test]
fn copy_sources_and_delete_revisions_are_passed_on() {
    let evs = parse(&report(
        r#"send-all="true""#,
        r#"<S:open-directory rev="1">
<S:add-directory name="d" copyfrom-path="/trunk/old" copyfrom-rev="4"/>
<S:add-file name="f" copyfrom-path="/trunk/a.txt" copyfrom-rev="3"><S:txdelta>U1ZOAA==</S:txdelta></S:add-file>
<S:delete-entry name="gone" rev="7"/><S:delete-entry name="gone2"/>
<S:delete-entry name="gone3" rev="-1"/>
</S:open-directory>"#,
    ))
    .unwrap();
    check(&evs);
    assert!(evs.iter().any(|e| matches!(e, UpdateEvent::AddDir { copy_from: Some((p, r)), .. } if p == "/trunk/old" && r.0 == 4)));
    assert!(evs.iter().any(|e| matches!(e, UpdateEvent::AddFile { copy_from: Some((p, r)), .. } if p == "/trunk/a.txt" && r.0 == 3)));
    assert!(evs.iter().any(|e| matches!(e, UpdateEvent::DeleteEntry { path, rev: Some(r) } if path.as_str() == "gone" && r.0 == 7)));
    assert!(evs.iter().any(
        |e| matches!(e, UpdateEvent::DeleteEntry { path, rev: None } if path.as_str() == "gone2")
    ));
    // A switch deletes with no revision: mod_dav_svn sends SVN_INVALID_REVNUM as "-1".
    assert!(evs.iter().any(
        |e| matches!(e, UpdateEvent::DeleteEntry { path, rev: None } if path.as_str() == "gone3")
    ));
    for bad in [
        r#"<S:add-file name="f" copyfrom-path="/a"/>"#,
        r#"<S:add-file name="f" copyfrom-rev="3"/>"#,
        r#"<S:add-file name="f" copyfrom-path="/a" copyfrom-rev="x"/>"#,
        r#"<S:open-file name="f" rev="1" copyfrom-path="/a" copyfrom-rev="3"/>"#,
        r#"<S:delete-entry name="g" rev="-2"/>"#,
        r#"<S:delete-entry name="g" rev="x"/>"#,
    ] {
        corrupt(&root(bad), bad);
    }
}

#[test]
fn skelta_fetches_contents_instead_of_leaving_empty_files() {
    let skelta = report(
        "",
        r#"<S:target-revision rev="2"/><S:open-directory rev="1"><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk</D:href></D:checked-in>
<S:add-file name="new.txt"><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk/new.txt</D:href></D:checked-in></S:add-file>
<S:open-file name="a.txt" rev="1"><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk/a.txt</D:href></D:checked-in></S:open-file>
</S:open-directory>"#,
    );
    let evs = parse(&skelta).unwrap();
    check(&evs);
    let fetched: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            UpdateEvent::FetchFile {
                path,
                href: Some(_),
                ..
            } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        fetched,
        ["new.txt"],
        "only added files; a.txt is opened and keeps its text"
    );
    // The fetch comes before the file closes, so the driver can finish the file with the text.
    let fetch = evs
        .iter()
        .position(
            |e| matches!(e, UpdateEvent::FetchFile { path, .. } if path.as_str() == "new.txt"),
        )
        .unwrap();
    let close = evs
        .iter()
        .position(
            |e| matches!(e, UpdateEvent::CloseFile { path, .. } if path.as_str() == "new.txt"),
        )
        .unwrap();
    assert!(fetch < close);
    // Without inline properties, added nodes also need their properties fetched.
    let props = evs
        .iter()
        .filter(|e| matches!(e, UpdateEvent::FetchProps { .. }))
        .count();
    assert_eq!(
        props, 1,
        "only the added file; opened nodes list their changes themselves"
    );

    // Send-all never synthesizes fetches; with inline properties neither does skelta.
    let send_all = report(
        r#"send-all="true" inline-props="true""#,
        r#"<S:open-directory rev="1"><S:add-file name="e"><S:txdelta>U1ZOAA==</S:txdelta></S:add-file></S:open-directory>"#,
    );
    assert!(!parse(&send_all).unwrap().iter().any(|e| matches!(
        e,
        UpdateEvent::FetchFile { .. } | UpdateEvent::FetchProps { .. }
    )));
    let inline = report(
        r#"inline-props="true""#,
        r#"<S:open-directory rev="1"><S:add-file name="e"/></S:open-directory>"#,
    );
    let evs = parse(&inline).unwrap();
    assert!(
        evs.iter()
            .any(|e| matches!(e, UpdateEvent::FetchFile { .. }))
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, UpdateEvent::FetchProps { .. }))
    );
}

#[test]
fn recorded_skelta_reports_fetch_every_added_file() {
    let evs = parse_update(&urls(), &load("update/checkout-skelta").resp_body).unwrap();
    check(&evs);
    let added = evs
        .iter()
        .filter(|e| matches!(e, UpdateEvent::AddFile { .. }))
        .count();
    let fetched = evs
        .iter()
        .filter(|e| matches!(e, UpdateEvent::FetchFile { href: Some(_), .. }))
        .count();
    assert_eq!((added, fetched), (4, 4));
}

#[test]
fn recorded_reports_are_valid_drives() {
    for name in [
        "checkout-send-all",
        "checkout-skelta",
        "update-send-all",
        "status-u",
        "switch",
        "update-rich",
        "update-rich-skelta",
        "update-mixed",
        "update-sparse",
        "status-u-rich",
    ] {
        check(&parse_update(&urls(), &load(&format!("update/{name}")).resp_body).unwrap());
    }
}

#[test]
fn unsafe_names_fail_the_update_naming_the_path() {
    for (name, shown) in [
        ("a:b", "a:b"),
        (".svn", ".svn"),
        ("CON", "CON"),
        ("..", ".."),
    ] {
        let doc = root(&format!(
            r#"<S:add-directory name="d"><S:add-file name="{name}"/></S:add-directory>"#
        ));
        let err = parse(&doc).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsafePath, "{name}");
        assert!(
            err.message.contains(&format!("d/{shown}")),
            "{}",
            err.message
        );
    }
}

#[test]
fn the_checker_catches_what_the_parser_would_have_rejected() {
    use svn_types::RelPath;
    let p = |s: &str| RelPath::new(s).unwrap();
    let mut c = DriveChecker::new();
    c.push(&UpdateEvent::ReportStart {
        send_all: true,
        inline_props: true,
    })
    .unwrap();
    c.push(&UpdateEvent::OpenDir {
        path: RelPath::root(),
        base_rev: None,
        checked_in: None,
    })
    .unwrap();
    assert!(
        c.push(&UpdateEvent::TxDeltaStart {
            path: p("x"),
            base_checksum: None
        })
        .is_err()
    );
    c.push(&UpdateEvent::CloseDir {
        path: RelPath::root(),
    })
    .unwrap();
    assert!(
        c.push(&UpdateEvent::OpenDir {
            path: RelPath::root(),
            base_rev: None,
            checked_in: None
        })
        .is_err()
    );
}

mod mutated {
    use super::*;
    use proptest::prelude::*;
    use svn_protocol::ResponseParser;
    use svn_protocol::update::UpdateParser;

    const NAMES: [&str; 5] = [
        "update/checkout-send-all",
        "update/checkout-skelta",
        "update/update-rich",
        "update/update-mixed",
        "update/update-sparse",
    ];

    proptest! {
        /// Whatever corruption the parser lets through must still be a valid editor drive.
        #[test]
        fn accepted_events_always_form_a_valid_drive(
            idx in 0usize..5,
            flips in proptest::collection::vec((0usize..65536, any::<u8>()), 0..6),
            cut in 0usize..65536,
            swap in proptest::option::of((0usize..65536, 0usize..65536)),
            step in 1usize..200,
        ) {
            let mut data = load(NAMES[idx]).resp_body;
            for (pos, b) in flips {
                let len = data.len();
                data[pos % len] = b;
            }
            if let Some((a, b)) = swap {
                // Move a slice around: reorders elements, the most likely way to break the drive.
                let (a, b) = (a % data.len(), b % data.len());
                let (lo, hi) = (a.min(b), a.max(b));
                let piece: Vec<u8> = data.drain(lo..hi).collect();
                let at = (cut % (data.len() + 1)).min(data.len());
                for (i, byte) in piece.into_iter().enumerate() {
                    data.insert(at + i, byte);
                }
            }
            let mut p = UpdateParser::new(urls());
            let mut checker = DriveChecker::new();
            for part in data.chunks(step) {
                match p.feed(part) {
                    Ok(events) => {
                        for e in events {
                            prop_assert!(checker.push(&e).is_ok(), "{:?}", e);
                        }
                    }
                    Err(_) => return Ok(()),
                }
            }
            if let Ok(events) = p.finish() {
                for e in events {
                    prop_assert!(checker.push(&e).is_ok(), "{:?}", e);
                }
                prop_assert!(checker.finish().is_ok());
            }
        }
    }
}

#[test]
fn checked_in_with_two_hrefs_is_corrupt() {
    let two = root(
        r#"<S:add-file name="f"><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk/f</D:href><D:href>/svn/repo/!svn/rvr/2/trunk/g</D:href></D:checked-in></S:add-file>"#,
    );
    corrupt(&two, "two hrefs");
    let twice = root(
        r#"<S:add-file name="f"><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk/f</D:href></D:checked-in><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk/f</D:href></D:checked-in></S:add-file>"#,
    );
    corrupt(&twice, "two checked-in elements");
    let one = root(
        r#"<S:add-file name="f"><D:checked-in><D:href>/svn/repo/!svn/rvr/2/trunk/f</D:href></D:checked-in></S:add-file>"#,
    );
    assert!(parse(&one).is_ok());
}

#[test]
fn skelta_does_not_refetch_opened_files_with_only_property_changes() {
    let doc = report(
        "",
        r#"<S:open-directory rev="1"><S:open-file name="a.txt" rev="1"><S:set-prop name="p">v</S:set-prop></S:open-file><S:add-file name="n.txt"/></S:open-directory>"#,
    );
    let evs = parse(&doc).unwrap();
    check(&evs);
    let fetched: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            UpdateEvent::FetchFile { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(fetched, ["n.txt"]);
}
