//! `status()` compared with `svn status --xml` on real working copies (svn 1.15.0 when
//! available; skipped otherwise unless `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use schema_support::*;
use svn_types::{CancelToken, RelPath};
use svn_wc::db::WcDb;
use svn_wc::status::{DbPristineSource, StatusOptions, status};

/// `path -> (item, props, copied, switched)` from `svn status --xml`.
fn svn_status(wc: &std::path::Path) -> BTreeMap<String, (String, String, bool)> {
    let out = Command::new("svn")
        .args(["status", "--xml", "--no-ignore"])
        .current_dir(wc)
        .env("LC_ALL", "en_US.UTF-8")
        .output()
        .expect("svn status");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let xml = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut map = BTreeMap::new();
    for entry in xml.split("<entry").skip(1) {
        let attr = |hay: &str, name: &str| {
            let key = format!(" {name}=\"");
            hay.find(&key).map(|i| {
                let rest = &hay[i + key.len()..];
                rest[..rest.find('"').expect("quote")].to_string()
            })
        };
        let path = attr(entry, "path").expect("path");
        let wc_status = &entry[entry.find("<wc-status").expect("wc-status")..];
        map.insert(
            path,
            (
                attr(wc_status, "item").expect("item"),
                attr(wc_status, "props").expect("props"),
                attr(wc_status, "copied").is_some(),
            ),
        );
    }
    map
}

fn ours(wc: &std::path::Path) -> BTreeMap<String, (String, String, bool)> {
    let db = WcDb::open(wc).expect("open");
    let source = DbPristineSource(&db);
    let opts = StatusOptions {
        no_ignore: true,
        ..StatusOptions::default()
    };
    status(&db, &source, &RelPath::root(), &opts, &CancelToken::new())
        .expect("status")
        .into_iter()
        .map(|e| {
            let path = if e.path.is_root() {
                ".".to_string()
            } else {
                e.path.as_str().to_string()
            };
            (
                path,
                (
                    e.xml_item().xml_word().to_string(),
                    e.xml_props().xml_word().to_string(),
                    e.copied,
                ),
            )
        })
        .collect()
}

#[test]
fn status_matches_svn_for_common_states() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let url = create_repo(base.path());
    let wc = checkout(base.path(), &url, "wc", &[]);
    fs::create_dir_all(wc.join("d")).expect("mkdir");
    for (f, body) in [
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
        ("c.txt", "c\n"),
        ("d/e.txt", "e\n"),
        ("gone.txt", "g\n"),
    ] {
        fs::write(wc.join(f), body).expect("write");
    }
    svn(
        &wc,
        &["add", "-q", "a.txt", "b.txt", "c.txt", "d", "gone.txt"],
    );
    svn(&wc, &["propset", "-q", "k", "v", "c.txt"]);
    svn(&wc, &["commit", "-q", "-m", "base"]);

    fs::write(wc.join("a.txt"), "changed\n").expect("modify"); // M
    svn(&wc, &["propset", "-q", "k", "x", "c.txt"]); // props modified
    svn(&wc, &["delete", "-q", "b.txt"]); // D
    fs::remove_file(wc.join("gone.txt")).expect("rm"); // missing
    fs::write(wc.join("new.txt"), "n\n").expect("write");
    svn(&wc, &["add", "-q", "new.txt"]); // A
    svn(&wc, &["copy", "-q", "c.txt", "copy.txt"]); // A +
    fs::write(wc.join("unversioned.txt"), "u\n").expect("write"); // ?
    fs::write(wc.join("junk.o"), "o\n").expect("write"); // I
    fs::create_dir_all(wc.join("d/sub")).expect("mkdir");
    fs::write(wc.join("d/sub/inner.txt"), "i\n").expect("write"); // ? (directory only)

    let mut expected = svn_status(&wc);
    let mut got = ours(&wc);
    // Compare only entries svn lists (svn also prints unversioned directories once).
    expected.retain(|_, _| true);
    got.retain(|p, _| expected.contains_key(p) || !p.contains('/'));
    assert_eq!(got, expected);
}
