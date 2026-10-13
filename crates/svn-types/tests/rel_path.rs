//! Validation rules for `RelPath::new` and `RelPath::from_server`.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use proptest::prelude::*;
use svn_types::{ErrorCode, RelPath};

fn rejected(s: &str) -> bool {
    matches!(RelPath::from_server(s), Err(e) if e.code == ErrorCode::UnsafePath)
}

#[test]
fn accepts_ordinary_paths() {
    for s in [
        "a",
        "a/b/c.txt",
        "trunk/src/main.rs",
        "dir with space/f",
        "a.b/c",
        ".gitignore",
        "v1.0/x",
    ] {
        let p = RelPath::from_server(s).expect(s);
        assert_eq!(p.as_str(), s);
    }
}

#[test]
fn rejects_traversal_and_structure() {
    for s in [
        "", "..", ".", "a/../b", "a/./b", "../a", "/a", "a/", "a//b", "a/b/..",
    ] {
        assert!(rejected(s), "{s:?} must be rejected");
    }
}

#[test]
fn rejects_backslash_colon_nul_and_controls() {
    for s in [
        "a\\b",
        "a:b",
        "a/b:stream",
        "a\0b",
        "a\nb",
        "a\u{7f}b",
        "a\tb",
        "C:/x",
    ] {
        assert!(rejected(s), "{s:?} must be rejected");
    }
}

#[test]
fn rejects_admin_dir_any_case_any_depth() {
    for s in [".svn", ".SVN", ".sVn/pristine", "a/.svn/wc.db", "a/b/.Svn"] {
        assert!(rejected(s), "{s:?} must be rejected");
    }
    assert!(RelPath::from_server(".svnignore").is_ok());
    assert!(RelPath::from_server("svn").is_ok());
}

#[test]
fn rejects_windows_reserved_names_on_every_platform() {
    for s in [
        "CON",
        "con",
        "PRN",
        "AUX",
        "NUL",
        "com1",
        "COM9",
        "LPT1",
        "lpt9",
        "a/CON.txt",
        "NUL.tar.gz",
        "Aux.c",
        "COM\u{00B9}",
        "con .txt",
        "CONIN$",
        "CONOUT$",
    ] {
        assert!(rejected(s), "{s:?} must be rejected");
    }
    for s in [
        "COM0", "COM10", "LPT", "CONSOLE", "NULL", "xCON", "a.CON", "COMM1",
    ] {
        assert!(RelPath::from_server(s).is_ok(), "{s:?} must be accepted");
    }
}

#[test]
fn rejects_trailing_dot_or_space() {
    for s in ["a.", "a ", "a/b.", "a/b /c", "a.."] {
        assert!(rejected(s), "{s:?} must be rejected");
    }
}

#[test]
fn normalizes_to_nfc() {
    let nfd = "e\u{0301}.txt";
    let nfc = "\u{00E9}.txt";
    let a = RelPath::from_server(nfd).expect("nfd");
    let b = RelPath::from_server(nfc).expect("nfc");
    assert_eq!(a, b);
    assert_eq!(a.as_str(), nfc);
    assert_eq!(
        RelPath::new("dir/e\u{0301}").expect("new").as_str(),
        "dir/\u{00E9}"
    );
}

#[test]
fn internal_constructor_is_lenient_about_names_but_not_structure() {
    assert!(RelPath::new("").expect("root").is_root());
    assert!(RelPath::new("a:b").is_ok());
    assert!(RelPath::new("CON").is_ok());
    assert!(RelPath::new("a/../b").is_err());
    assert!(RelPath::new("/a").is_err());
    assert!(RelPath::new("a//b").is_err());
}

#[test]
fn navigation_helpers() {
    let p = RelPath::new("a/b/c").expect("path");
    assert_eq!(p.file_name(), Some("c"));
    assert_eq!(p.parent().expect("parent").as_str(), "a/b");
    assert_eq!(
        RelPath::new("a").expect("a").parent().expect("p"),
        RelPath::root()
    );
    assert!(RelPath::root().parent().is_none());
    assert_eq!(p.components().collect::<Vec<_>>(), ["a", "b", "c"]);
    assert!(p.starts_with(&RelPath::new("a/b").expect("ab")));
    assert!(!p.starts_with(&RelPath::new("a/bc").expect("abc")));
    assert!(p.starts_with(&RelPath::root()));
    assert_eq!(RelPath::root().child("x").expect("child").as_str(), "x");
    assert!(p.child("a/b").is_err());
}

proptest! {
    /// Whatever the input, an accepted server path never contains an escape primitive.
    #[test]
    fn accepted_server_paths_are_always_safe(s in "\\PC{0,24}(/\\PC{0,12}){0,4}") {
        if let Ok(p) = RelPath::from_server(&s) {
            for seg in p.components() {
                prop_assert!(seg != "." && seg != "..");
                prop_assert!(!seg.eq_ignore_ascii_case(".svn"));
                prop_assert!(!seg.contains(['\\', ':', '\0']));
                prop_assert!(!seg.ends_with(['.', ' ']));
            }
            prop_assert!(!p.as_str().starts_with('/') && !p.as_str().ends_with('/'));
        }
    }
}
