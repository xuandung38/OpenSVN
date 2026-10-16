//! `propset` refuses and accepts Subversion properties exactly as `svn propset` 1.15 does
//! (without `--force`): unknown `svn:` names, where a property applies, `svn:eol-style`,
//! `svn:mime-type`, `svn:externals` and `svn:mergeinfo` values. Each case runs on an svn checkout
//! and on ours; the outcome must match, and the wc.db rows must be identical at the end.
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use support::*;
use svn_client::local::{self, PropSetOptions};
use svn_types::{CancelToken, ErrorCode};

/// `(path, name, value)`; `None` deletes.
type Step<'a> = (&'a str, &'a str, Option<&'a str>);

const STEPS: &[Step<'static>] = &[
    // Names svn does not know are refused without --force; other namespaces are free.
    ("d/f1", "svn:foo", Some("x")),
    ("d/f1", "svn:wc:ra_dav", Some("x")),
    ("d/f1", "my:own", Some("anything at all")),
    // Where a property applies.
    ("d", "svn:eol-style", Some("LF")),
    ("d/f1", "svn:ignore", Some("*.o")),
    ("d/f2", "svn:externals", Some("^/d ext")),
    // svn:eol-style values.
    ("d/f1", "svn:eol-style", Some("bogus")),
    ("d/f1", "svn:eol-style", Some("lf")),
    ("d/f1", "svn:eol-style", Some("LF")),
    // svn:mime-type values.
    ("d/f2", "svn:mime-type", Some("textplain")),
    ("d/f2", "svn:mime-type", Some("text/pl(ain")),
    ("d/f2", "svn:mime-type", Some("/plain")),
    ("d/f2", "svn:mime-type", Some("text/plain; charset=UTF-8")),
    // No line endings on a file svn treats as binary.
    ("top.txt", "svn:mime-type", Some("application/octet-stream")),
    ("top.txt", "svn:eol-style", Some("native")),
    // Boolean properties become `*`.
    ("d/sub/g", "svn:needs-lock", Some("yes")),
    ("d/sub/g", "svn:executable", Some("on")),
    // svn:externals values.
    ("e", "svn:externals", Some("a b c d e")),
    (
        "e",
        "svn:externals",
        Some("http://h.example.com/a http://h.example.com/b"),
    ),
    ("e", "svn:externals", Some("^/d ../outside")),
    ("e", "svn:externals", Some("^/d x\n^/e x")),
    ("e", "svn:externals", Some("^/d ext\n-r1 ^/e ext2")),
    // svn:mergeinfo values (already canonical, as svn would store them).
    ("d", "svn:mergeinfo", Some("/trunk:3-1")),
    ("d", "svn:mergeinfo", Some("/trunk:0")),
    ("d", "svn:mergeinfo", Some("/trunk")),
    ("d", "svn:mergeinfo", Some("/trunk:1")),
    // Deleting is never checked.
    ("d/f2", "svn:mime-type", None),
];

#[test]
fn propset_accepts_and_refuses_what_svn_does() {
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    let db = open(&b);
    let cancel = CancelToken::new();
    for (path, name, value) in STEPS {
        let mut cmd = fx.oracle.svn();
        match value {
            Some(v) => cmd.args(["propset", "-q", name, v, path]),
            None => cmd.args(["propdel", "-q", name, path]),
        };
        let out = cmd.current_dir(&a).output().expect("svn");
        let svn_ok = out.status.success();
        let ours = local::propset(
            &db,
            &cancel,
            &rp(path),
            name,
            value.map(str::as_bytes),
            PropSetOptions::default(),
        );
        assert_eq!(
            ours.is_ok(),
            svn_ok,
            "{path} {name}={value:?}: svn says {:?}, we say {ours:?}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        if let Err(e) = ours {
            assert_eq!(e.code, ErrorCode::WcInvalidOperation, "{e}");
        }
    }
    drop(db);
    assert_rows_equal("propset checks", &dump_rows(&a), &dump_rows(&b));
}

#[test]
fn skip_checks_stores_what_svn_force_stores() {
    let Some(fx) = fixture() else { return };
    let a = checkout(&fx.oracle, &fx.base, &fx.url, "svn-side");
    let b = checkout(&fx.oracle, &fx.base, &fx.url, "our-side");
    svn(
        &fx.oracle,
        &a,
        &["propset", "-q", "--force", "svn:foo", "x", "d/f1"],
    );
    let db = open(&b);
    local::propset(
        &db,
        &CancelToken::new(),
        &rp("d/f1"),
        "svn:foo",
        Some(b"x"),
        PropSetOptions { skip_checks: true },
    )
    .expect("forced");
    drop(db);
    assert_rows_equal("forced", &dump_rows(&a), &dump_rows(&b));
}

#[test]
fn value_checks_match_svn_rules() {
    use local::props::{mime_type_is_binary, validate_mergeinfo, validate_mime_type};
    assert!(validate_mime_type("text/plain").is_ok());
    assert!(validate_mime_type("image/svg+xml; charset=utf-8").is_ok());
    assert!(validate_mime_type("").is_err());
    assert!(validate_mime_type("text").is_err());
    assert!(
        validate_mime_type("te xt/plain").is_err(),
        "space ends the media type"
    );
    assert!(validate_mime_type("text/pl\u{e9}in").is_err());
    assert!(validate_mime_type("text/plain;\u{1}").is_err());
    assert!(!mime_type_is_binary("text/x-c"));
    assert!(!mime_type_is_binary("image/x-xbitmap"));
    assert!(mime_type_is_binary("application/octet-stream"));
    assert!(validate_mergeinfo("/a:1-2,4*\n/b/c:7").is_ok());
    assert!(validate_mergeinfo("a:1").is_err());
    assert!(validate_mergeinfo("/a:2-2").is_err());
    assert!(validate_mergeinfo("/a:").is_err());
    assert!(validate_mergeinfo("/a:x").is_err());
}
