//! `.prej` text tests. Golden files are written by svn 1.15.0
//! (`tests/compat/scenarios/wc/conflicts/prej/generate.py`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use super::*;
use crate::conflicts::ConflictSkel;
use crate::skel::{Skel, parse_proplist};

fn fixture(name: &str, ext: &str) -> Option<Vec<u8>> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/compat/scenarios/wc/conflicts/prej")
        .join(format!("{name}.{ext}"));
    std::fs::read(path).ok()
}

fn generated(name: &str) -> Vec<u8> {
    let skel = ConflictSkel::decode(&fixture(name, "skel").unwrap()).unwrap();
    let operation = skel.operation.as_ref().unwrap().operation.clone();
    let prop = skel.prop().unwrap();
    let pristine =
        fixture(name, "pristine").map(|b| parse_proplist(&Skel::parse(&b).unwrap()).unwrap());
    prej_text(&operation, prop, pristine.as_deref())
}

#[test]
fn matches_the_reject_files_svn_wrote() {
    for name in [
        "update_multiline",
        "update_both_add",
        "update_delete_vs_modify",
        "update_modify_vs_delete",
        "update_binary",
        "update_non_ascii",
        "update_single_line",
        "merge_single_line",
    ] {
        let expected = fixture(name, "prej").unwrap();
        let got = generated(name);
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&expected),
            "{name}"
        );
        assert_eq!(got, expected, "{name}: bytes");
    }
}

fn values<'a>(
    original: Option<&'a str>,
    mine: Option<&'a str>,
    incoming: Option<&'a str>,
    base: Option<&'a str>,
) -> PropValues<'a> {
    PropValues {
        original: original.map(str::as_bytes),
        mine: mine.map(str::as_bytes),
        incoming: incoming.map(str::as_bytes),
        incoming_base: base.map(str::as_bytes),
    }
}

fn first_two_lines(v: &PropValues<'_>) -> String {
    String::from_utf8(conflict_description("p", v))
        .unwrap()
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn every_message_branch() {
    let cases: Vec<(PropValues<'_>, &str)> = vec![
        // Adding a property.
        (
            values(None, Some("a"), Some("b"), None),
            "property already exists",
        ),
        (values(Some("x"), None, Some("b"), None), "locally deleted"),
        // Deleting a property.
        (values(None, Some("a"), None, Some("x")), "locally added"),
        (
            values(Some("x"), Some("y"), None, Some("x")),
            "locally modified",
        ),
        (
            values(Some("z"), None, None, Some("x")),
            "locally deleted and had a different value",
        ),
        (
            values(Some("z"), Some("y"), None, Some("x")),
            "local property value is different",
        ),
        // Changing a property.
        (
            values(Some("x"), Some("x"), Some("b"), Some("a")),
            "conflicts with the incoming change",
        ),
        (
            values(Some("x"), Some("y"), Some("b"), Some("a")),
            "already been locally changed",
        ),
        (
            values(Some("x"), None, Some("b"), Some("a")),
            "has been locally deleted",
        ),
        (
            values(None, Some("y"), Some("b"), Some("a")),
            "locally added with a different value",
        ),
        (
            values(None, None, Some("b"), Some("a")),
            "does not exist locally",
        ),
    ];
    for (v, needle) in cases {
        let text = first_two_lines(&v);
        assert!(text.contains(needle), "{needle}: {text}");
    }
}

#[test]
fn fallback_prints_plain_values_when_the_merge_has_no_conflict() {
    // Identical local and incoming values: diff3 reports no conflict, so svn prints both.
    let v = values(Some("o"), Some("same"), Some("same"), Some("old"));
    let text = String::from_utf8(conflict_description("p", &v)).unwrap();
    assert!(
        text.ends_with("Local property value:\nsame\nIncoming property value:\nsame\n"),
        "{text}"
    );
}

#[test]
fn binary_detection_matches_svn() {
    assert!(is_binary(b"a\0b"));
    assert!(!is_binary(b"plain text\n"));
    assert!(!is_binary(&[0xEF, 0xBB, 0xBF]));
    assert!(!is_binary(b""));
    assert!(is_binary(&[0x80; 10]));
    assert!(!is_binary(&[0x80, b'a', b'b', b'c', b'd']));
}

#[test]
fn fuzzy_escape_follows_svn() {
    assert_eq!(fuzzy_escape(b"plain\nline"), "plain\nline");
    assert_eq!(fuzzy_escape("café ü".as_bytes()), "cafe u");
    assert_eq!(fuzzy_escape(b"a\r\nb\rc\td\x01e"), "a\nb\nc de");
    assert_eq!(fuzzy_escape(b"ok\xFFbad\xC3"), "ok?\\FFbad?\\C3");
    assert_eq!(fuzzy_escape("日本".as_bytes()), "{U+65E5}{U+672C}");
    assert_eq!(fuzzy_escape("😀".as_bytes()), "{U+01F600}");
}

#[test]
fn names_are_rendered_in_sorted_order_and_old_props_select_the_reference() {
    let conflict = PropConflict {
        markers: vec![],
        names: vec!["b".into(), "a".into()],
        old_props: Some(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"1".to_vec()),
        ]),
        mine_props: Some(vec![
            (b"a".to_vec(), b"2".to_vec()),
            (b"b".to_vec(), b"3".to_vec()),
        ]),
        their_props: Some(vec![
            (b"a".to_vec(), b"4".to_vec()),
            (b"b".to_vec(), b"5".to_vec()),
        ]),
    };
    let text = String::from_utf8(prej_text(&Operation::Update, &conflict, None)).unwrap();
    let (a, b) = (
        text.find("property 'a'").unwrap(),
        text.find("property 'b'").unwrap(),
    );
    assert!(a < b, "{text}");
    // For a merge the reference value is the pristine one, not the incoming "changed from".
    let pristine = vec![(b"a".to_vec(), b"2".to_vec())];
    let merge =
        String::from_utf8(prej_text(&Operation::Merge, &conflict, Some(&pristine))).unwrap();
    assert!(
        merge.contains("conflicts with the incoming change"),
        "{merge}"
    );
}
