//! update-report parsing against recorded send-all and skelta answers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{DATA_SETS, for_each_set, load, run_split, urls, use_set};
use proptest::prelude::*;
use svn_protocol::update::{UpdateEvent, UpdateParser, parse_update};
use svn_protocol::{HrefKind, ResponseParser};
use svn_types::{ErrorCode, RelPath, Revnum};

fn events(name: &str) -> Vec<UpdateEvent> {
    parse_update(&urls(), &load(name).resp_body).unwrap()
}

/// Merges neighbouring delta chunks so that chunking does not affect comparisons.
fn coalesce(events: Vec<UpdateEvent>) -> Vec<UpdateEvent> {
    let mut out: Vec<UpdateEvent> = Vec::new();
    for e in events {
        match (out.last_mut(), e) {
            (
                Some(UpdateEvent::TxDeltaChunk { data: prev, .. }),
                UpdateEvent::TxDeltaChunk { data, .. },
            ) => {
                let mut joined = prev.to_vec();
                joined.extend_from_slice(&data);
                *prev = joined.into();
            }
            (_, e) => out.push(e),
        }
    }
    out
}

/// Opens and closes balance, in order, and the stream starts with the target revision.
fn assert_well_formed(evs: &[UpdateEvent]) {
    assert!(
        matches!(evs.first(), Some(UpdateEvent::ReportStart { .. }))
            && matches!(evs.get(1), Some(UpdateEvent::TargetRevision(_))),
        "{:?}",
        evs.get(..2)
    );
    let mut stack: Vec<(RelPath, bool)> = Vec::new();
    for e in evs {
        match e {
            UpdateEvent::OpenDir { path, .. } | UpdateEvent::AddDir { path, .. } => {
                stack.push((path.clone(), true))
            }
            UpdateEvent::OpenFile { path, .. } | UpdateEvent::AddFile { path, .. } => {
                stack.push((path.clone(), false))
            }
            UpdateEvent::CloseDir { path } => assert_eq!(stack.pop(), Some((path.clone(), true))),
            UpdateEvent::CloseFile { path, .. } => {
                assert_eq!(stack.pop(), Some((path.clone(), false)))
            }
            UpdateEvent::SetProp { path, .. }
            | UpdateEvent::RemoveProp { path, .. }
            | UpdateEvent::TxDeltaStart { path, .. }
            | UpdateEvent::TxDeltaChunk { path, .. }
            | UpdateEvent::TxDeltaEnd { path } => assert_eq!(&stack.last().unwrap().0, path),
            _ => {}
        }
    }
    assert!(stack.is_empty(), "unclosed: {stack:?}");
}

fn added_files(evs: &[UpdateEvent]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| {
            if let UpdateEvent::AddFile { path, .. } = e {
                Some(path.as_str().to_owned())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn every_recorded_report_is_well_formed() {
    for name in [
        "update/checkout-send-all",
        "update/checkout-skelta",
        "update/update-send-all",
        "update/status-u",
        "update/switch",
        "update/update-rich",
        "update/update-rich-skelta",
        "update/update-mixed",
        "update/update-sparse",
        "update/status-u-rich",
    ] {
        assert_well_formed(&events(name));
    }
}

#[test]
fn send_all_checkout_carries_text_deltas() {
    let evs = events("update/checkout-send-all");
    assert_eq!(
        evs[0],
        UpdateEvent::ReportStart {
            send_all: true,
            inline_props: true
        }
    );
    assert_eq!(evs[1], UpdateEvent::TargetRevision(Revnum(5)));
    let UpdateEvent::OpenDir {
        path,
        base_rev,
        checked_in,
    } = &evs[2]
    else {
        panic!("{:?}", evs[2])
    };
    assert!(path.is_root());
    assert_eq!(*base_rev, Some(Revnum(5)));
    let href = checked_in.as_ref().expect("root checked-in");
    assert!(matches!(&href.kind, HrefKind::RevRoot { path, .. } if path.as_str() == "trunk"));

    let mut files = added_files(&evs);
    files.sort();
    assert_eq!(
        files,
        [
            "a.txt",
            "src/b.txt",
            "src/deep/c.cfg",
            "unicod\u{e9} file.txt"
        ]
    );

    let evs = coalesce(evs);
    let delta = evs.iter().find_map(|e| match e {
        UpdateEvent::TxDeltaChunk { path, data } if path.as_str() == "a.txt" => Some(data.clone()),
        _ => None,
    });
    let delta = delta.expect("a.txt text delta");
    assert_eq!(&delta[..3], b"SVN", "svndiff magic");
    // The final checksum arrives with the file and is the MD5 of the new text.
    let close = evs.iter().find_map(|e| match e {
        UpdateEvent::CloseFile { path, md5 } if path.as_str() == "a.txt" => Some(*md5),
        _ => None,
    });
    assert!(close.unwrap().is_some());
}

#[test]
fn properties_and_entry_props_are_delivered() {
    let evs = events("update/checkout-send-all");
    let prop = |file: &str, name: &str| {
        evs.iter().find_map(|e| match e {
            UpdateEvent::SetProp {
                path,
                name: n,
                value,
            } if path.as_str() == file && n == name => Some(value.clone()),
            _ => None,
        })
    };
    assert_eq!(
        prop("a.txt", "custom:tag").unwrap(),
        br#"v<1>&"q""#.as_slice()
    );
    assert_eq!(
        prop("src/b.txt", "svn:executable").unwrap(),
        b"*".as_slice()
    );
    assert_eq!(
        prop("a.txt", "svn:entry:last-author").unwrap(),
        b"alice".as_slice()
    );
    assert!(prop("a.txt", "svn:entry:committed-rev").is_some());
}

#[test]
fn skelta_has_no_inline_text_and_gives_urls_to_fetch() {
    let evs = events("update/checkout-skelta");
    assert!(!evs.iter().any(|e| matches!(
        e,
        UpdateEvent::TxDeltaStart { .. } | UpdateEvent::TxDeltaChunk { .. }
    )));
    for e in &evs {
        if let UpdateEvent::AddFile {
            path,
            checked_in,
            sha1,
            ..
        } = e
        {
            let href = checked_in
                .as_ref()
                .unwrap_or_else(|| panic!("{path} has no checked-in"));
            assert!(
                matches!(&href.kind, HrefKind::RevRoot { path: p, .. } if p.as_str() == format!("trunk/{path}")),
                "{path}: {:?}",
                href.kind
            );
            assert!(sha1.is_some());
        }
    }
    assert_eq!(added_files(&evs).len(), 4);
}

#[test]
fn update_with_changed_file_has_base_checksum() {
    let evs = events("update/update-send-all");
    let start = evs.iter().find_map(|e| match e {
        UpdateEvent::TxDeltaStart {
            path,
            base_checksum,
        } if path.as_str() == "a.txt" => Some(*base_checksum),
        _ => None,
    });
    assert!(start.unwrap().is_some());
    assert!(evs.iter().any(|e| matches!(e, UpdateEvent::OpenFile { path, base_rev: Some(_), .. } if path.as_str() == "a.txt")));
}

#[test]
fn deletions_props_and_copies_in_a_rich_update() {
    let evs = events("update/update-rich");
    assert!(evs.iter().any(
        |e| matches!(e, UpdateEvent::DeleteEntry { path, .. } if path.as_str() == "src/b.txt")
    ));
    assert!(evs.iter().any(|e| matches!(e, UpdateEvent::SetProp { name, value, .. } if name == "svn:eol-style" && value.as_ref() == b"native")));
    assert!(evs.iter().any(|e| matches!(e, UpdateEvent::SetProp { name, value, .. } if name == "custom:note" && value.as_ref() == b"x<y")));
    assert!(added_files(&evs).iter().any(|f| f == "copy.txt"));
    // An empty file's delta is the bare svndiff header.
    assert!(coalesce(evs).iter().any(|e| matches!(e, UpdateEvent::TxDeltaChunk { path, data } if path.as_str() == "fresh/empty.txt" && data.len() == 4)));
}

#[test]
fn rich_update_in_skelta_mode_matches_the_tree_changes() {
    let evs = events("update/update-rich-skelta");
    assert!(evs.iter().any(
        |e| matches!(e, UpdateEvent::DeleteEntry { path, .. } if path.as_str() == "src/b.txt")
    ));
    assert!(
        !evs.iter()
            .any(|e| matches!(e, UpdateEvent::TxDeltaChunk { .. }))
    );
    assert!(added_files(&evs).iter().any(|f| f == "fresh/f.txt"));
}

#[test]
fn update_target_makes_the_root_directory_anonymous() {
    let evs = events("update/update-sparse");
    let dirs: Vec<String> = evs
        .iter()
        .filter_map(|e| {
            if let UpdateEvent::OpenDir { path, .. } = e {
                Some(path.as_str().to_owned())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(dirs, ["", "fresh"]);
}

#[test]
fn status_reports_have_empty_deltas() {
    let evs = coalesce(events("update/status-u"));
    for e in &evs {
        if let UpdateEvent::TxDeltaChunk { data, .. } = e {
            assert_eq!(data.as_ref(), b"SVN\0");
        }
    }
}

#[test]
fn synthetic_skelta_fetch_elements_and_late_checked_in() {
    for_each_set(|| {
        let doc = r#"<S:update-report xmlns:S="svn:" xmlns:D="DAV:" xmlns:V="http://subversion.tigris.org/xmlns/dav/">
<S:target-revision rev="7"/>
<S:open-directory rev="3">
 <S:fetch-props/>
 <S:add-file name="f.txt"><D:checked-in><D:href>/svn/repo/!svn/rvr/7/trunk/f.txt</D:href></D:checked-in>
  <S:fetch-file base-checksum="d41d8cd98f00b204e9800998ecf8427e"/><S:fetch-props/>
  <S:set-prop name="svn:mime-type" encoding="base64">dGV4dC9wbGFpbg==</S:set-prop><S:remove-prop name="gone"/>
  <S:prop><V:md5-checksum>d41d8cd98f00b204e9800998ecf8427e</V:md5-checksum></S:prop></S:add-file>
 <S:absent-file name="secret.txt"/><S:absent-directory name="hidden"/>
 <S:unknown-future-thing><S:nested/></S:unknown-future-thing>
</S:open-directory></S:update-report>"#;
        let evs = parse_update(&urls(), doc.as_bytes()).unwrap();
        assert_well_formed(&evs);
        assert!(
            evs.iter().any(
                |e| matches!(e, UpdateEvent::FetchProps { path, href: None } if path.is_root())
            )
        );
        assert!(evs.iter().any(|e| matches!(e, UpdateEvent::FetchFile { path, href: Some(_), base_checksum: Some(_) } if path.as_str() == "f.txt")));
        assert!(evs.iter().any(|e| matches!(e, UpdateEvent::SetProp { name, value, .. } if name == "svn:mime-type" && value.as_ref() == b"text/plain")));
        assert!(
            evs.iter()
                .any(|e| matches!(e, UpdateEvent::RemoveProp { name, .. } if name == "gone"))
        );
        assert!(evs.iter().any(
            |e| matches!(e, UpdateEvent::AbsentFile { path } if path.as_str() == "secret.txt")
        ));
        assert!(
            evs.iter()
                .any(|e| matches!(e, UpdateEvent::AbsentDir { path } if path.as_str() == "hidden"))
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e, UpdateEvent::CloseFile { md5: Some(_), .. }))
        );
    });
}

#[test]
fn hostile_reports_are_rejected() {
    for_each_set(|| {
        let wrap = |inner: &str| {
            format!(
                r#"<S:update-report xmlns:S="svn:" xmlns:D="DAV:" xmlns:V="http://subversion.tigris.org/xmlns/dav/"><S:target-revision rev="1"/><S:open-directory rev="1">{inner}</S:open-directory></S:update-report>"#
            )
        };
        let bad_names = [
            "..", ".", ".svn", ".SVN", "a/b", "../x", "CON", "aux.txt", "a:b", "a\\b", "x ", "x.",
            "",
        ];
        for name in bad_names {
            for tag in [
                "add-file",
                "add-directory",
                "open-file",
                "delete-entry",
                "absent-file",
            ] {
                let doc = wrap(&format!(r#"<S:{tag} name="{name}"/>"#));
                let err = parse_update(&urls(), doc.as_bytes()).err();
                assert!(err.is_some(), "{tag} name={name:?} was accepted");
            }
        }
        let bad_hrefs = [
            "http://evil.test/svn/repo/!svn/rvr/1/x",
            "/svn/repo/!svn/rvr/1/../../../etc/passwd",
            "/svn/repo/!svn/bc/1/trunk",
            "/elsewhere/file",
            "relative/path",
        ];
        for href in bad_hrefs {
            let doc = wrap(&format!(
                r#"<S:add-file name="f"><D:checked-in><D:href>{href}</D:href></D:checked-in></S:add-file>"#
            ));
            let err = parse_update(&urls(), doc.as_bytes()).unwrap_err();
            assert_eq!(err.code, ErrorCode::UnsafePath, "{href}");
        }
        for inner in [
            r#"<S:add-file name="f"><S:txdelta>!!!!</S:txdelta></S:add-file>"#,
            r#"<S:add-file name="f"><S:txdelta>U1Z</S:txdelta></S:add-file>"#,
            r#"<S:add-file name="f" sha1-checksum="xyz"/>"#,
            r#"<S:add-file name="f"><S:set-prop name="p" encoding="base64">%%%</S:set-prop></S:add-file>"#,
            r#"<S:set-prop>v</S:set-prop>"#,
            r#"<S:add-file name="f"><V:md5-checksum>nothex</V:md5-checksum></S:add-file>"#,
        ] {
            assert!(
                parse_update(&urls(), wrap(inner).as_bytes()).is_err(),
                "{inner}"
            );
        }
        assert!(parse_update(&urls(), b"<html/>").is_err());
        // Truncated before the directory closes.
        let truncated = br#"<S:update-report xmlns:S="svn:"><S:open-directory rev="1">"#;
        assert!(parse_update(&urls(), truncated).is_err());
    });
}

#[test]
fn nesting_beyond_the_limit_is_an_error() {
    for_each_set(|| {
        let mut doc = String::from(r#"<S:update-report xmlns:S="svn:">"#);
        doc.push_str(r#"<S:open-directory rev="1">"#);
        for _ in 0..300 {
            doc.push_str(r#"<S:add-directory name="d">"#);
        }
        assert!(parse_update(&urls(), doc.as_bytes()).is_err());
    });
}

#[test]
fn deltas_stream_before_the_document_ends() {
    for_each_set(|| {
        let f = load("update/checkout-send-all");
        let mut p = UpdateParser::new(urls());
        let cut = f.resp_body.len() / 2;
        let early = p.feed(&f.resp_body[..cut]).unwrap();
        assert!(
            early
                .iter()
                .any(|e| matches!(e, UpdateEvent::AddFile { .. }))
        );
        p.feed(&f.resp_body[cut..]).unwrap();
        p.finish().unwrap();
    });
}

proptest! {
    #[test]
    fn chunking_never_changes_the_events(cuts in proptest::collection::vec(0usize..32768, 0..16)) {
        for set in DATA_SETS {
            use_set(set);
            for name in ["update/checkout-send-all", "update/update-rich", "update/checkout-skelta", "update/update-mixed"] {
                let f = load(name);
                let whole = coalesce(parse_update(&urls(), &f.resp_body).unwrap());
                let split = coalesce(run_split(UpdateParser::new(urls()), &f.resp_body, &cuts));
                prop_assert_eq!(whole, split);
            }
        }
    }

    #[test]
    fn one_byte_pieces_and_odd_base64_boundaries(step in 1usize..40) {
        for set in DATA_SETS {
            use_set(set);
            let f = load("update/update-rich");
            let whole = coalesce(parse_update(&urls(), &f.resp_body).unwrap());
            let mut p = UpdateParser::new(urls());
            let mut got = Vec::new();
            for piece in f.resp_body.chunks(step) {
                got.extend(p.feed(piece).unwrap());
            }
            got.extend(p.finish().unwrap());
            prop_assert_eq!(whole, coalesce(got));
        }
    }
}
