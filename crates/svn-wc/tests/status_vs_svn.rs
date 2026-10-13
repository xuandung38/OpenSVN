//! `status()` compared with `svn status --xml` on real working copies built by the local svn
//! (1.14 or newer; skipped otherwise unless `OPENSVN_REQUIRE_SVN=1`).
//!
//! Every scenario is compared twice: the default report (`svn status --xml --no-ignore
//! --ignore-externals`) and the full one (`-v`, i.e. `StatusOptions::get_all`). Each entry is
//! compared on `item`, `props`, `copied`, `switched`, `file-external`, `tree-conflicted`,
//! `moved-to`, the `revision` attribute and the last-changed revision.
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use schema_support::*;
use svn_types::{CancelToken, RelPath};
use svn_wc::db::WcDb;
use svn_wc::status::{DbPristineSource, StatusOptions, status};

/// What one status entry is compared on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    item: String,
    props: String,
    copied: bool,
    switched: bool,
    file_external: bool,
    tree_conflicted: bool,
    moved_to: Option<String>,
    revision: Option<String>,
    changed_rev: Option<String>,
}

type Report = BTreeMap<String, Row>;

fn attr(hay: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    hay.find(&key).map(|i| {
        let rest = &hay[i + key.len()..];
        rest[..rest.find('"').expect("quote")].to_string()
    })
}

fn flag(hay: &str, name: &str) -> bool {
    attr(hay, name).is_some_and(|v| v == "true")
}

/// `svn status --xml` of the whole working copy, externals not descended into.
fn svn_report(wc: &Path, verbose: bool) -> Report {
    let mut args = vec!["status", "--xml", "--no-ignore", "--ignore-externals"];
    if verbose {
        args.push("-v");
    }
    let xml = svn_stdout(wc, &args);
    let mut map = Report::new();
    for entry in xml.split("<entry").skip(1) {
        let entry = &entry[..entry.find("</entry>").expect("entry end")];
        let path = attr(entry, "path").expect("path");
        let wc_status = &entry[entry.find("<wc-status").expect("wc-status")..];
        let head = &wc_status[..wc_status.find('>').expect("tag end")];
        let changed_rev = wc_status
            .find("<commit")
            .and_then(|i| attr(&wc_status[i..], "revision"));
        map.insert(
            path,
            Row {
                item: attr(head, "item").expect("item"),
                props: attr(head, "props").expect("props"),
                copied: flag(head, "copied"),
                switched: flag(head, "switched"),
                file_external: flag(head, "file-external"),
                tree_conflicted: flag(head, "tree-conflicted"),
                moved_to: attr(head, "moved-to"),
                revision: attr(head, "revision"),
                changed_rev,
            },
        );
    }
    map
}

/// Our report, rendered the way `svn status --xml` prints it (`print_status_xml`).
fn our_report(wc: &Path, verbose: bool) -> Report {
    let db = WcDb::open(wc).expect("open");
    let source = DbPristineSource(&db);
    let opts = StatusOptions {
        no_ignore: true,
        get_all: verbose,
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
            let row = Row {
                item: e.xml_item().xml_word().to_string(),
                props: e.xml_props().xml_word().to_string(),
                copied: e.copied,
                switched: e.switched,
                file_external: e.file_external,
                tree_conflicted: e.tree_conflicted,
                moved_to: e.moved_to.as_ref().map(|m| m.as_str().to_string()),
                revision: e.xml_revision().map(|r| r.to_string()),
                changed_rev: e.changed_rev.map(|r| r.to_string()),
            };
            (path, row)
        })
        .collect()
}

/// Asserts both reports (default and `-v`) equal svn's, with a per-path diff on failure.
fn assert_status_matches(wc: &Path) {
    for verbose in [false, true] {
        let expected = svn_report(wc, verbose);
        let got = our_report(wc, verbose);
        if expected != got {
            let mut diff = String::new();
            let keys: BTreeSet<_> = expected.keys().chain(got.keys()).collect();
            for k in keys {
                let (e, g) = (expected.get(k), got.get(k));
                if e != g {
                    diff.push_str(&format!("{k}\n  svn:  {e:?}\n  ours: {g:?}\n"));
                }
            }
            panic!("status mismatch (verbose = {verbose}):\n{diff}");
        }
    }
}

/// Guards against vacuous scenarios: svn itself must report `path` with this `item` (and, when
/// given, `props`), so the comparison really covers the state the scenario claims to build.
fn assert_svn_reports(wc: &Path, path: &str, item: &str, check: impl Fn(&Row) -> bool) {
    let report = svn_report(wc, true);
    let row = report
        .get(path)
        .unwrap_or_else(|| panic!("svn does not report {path}: {report:#?}"));
    assert!(
        row.item == item && check(row),
        "scenario setup: svn reports {path} as {row:?}, expected item {item}"
    );
}

fn write(wc: &Path, rel: &str, body: &str) {
    let path = wc.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, body).expect("write");
}

/// A repository with `trunk/{a,b,c}.txt`, `trunk/d/e.txt`, `trunk/d/f/g.txt` and `branches/`
/// committed, plus a working copy of `trunk` at `base/wc`. Returns `(repo_url, wc)`.
fn trunk_wc(base: &Path) -> (String, PathBuf) {
    let url = create_repo(base);
    let seed = checkout(base, &url, "seed", &[]);
    for (f, body) in [
        ("trunk/a.txt", "a\n"),
        ("trunk/b.txt", "b\n"),
        ("trunk/c.txt", "c\n"),
        ("trunk/d/e.txt", "e\n"),
        ("trunk/d/f/g.txt", "g\n"),
    ] {
        write(&seed, f, body);
    }
    fs::create_dir_all(seed.join("branches")).expect("mkdir");
    svn(&seed, &["add", "-q", "trunk", "branches"]);
    svn(&seed, &["propset", "-q", "k", "v", "trunk/c.txt"]);
    svn(&seed, &["commit", "-q", "-m", "seed"]);
    let wc = checkout(base, &format!("{url}/trunk"), "wc", &[]);
    (url, wc)
}

#[test]
fn status_matches_svn_for_common_states() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let url = create_repo(base.path());
    let wc = checkout(base.path(), &url, "wc", &[]);
    for (f, body) in [
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
        ("c.txt", "c\n"),
        ("d/e.txt", "e\n"),
        ("gone.txt", "g\n"),
        ("mv.txt", "m\n"),
        ("cl.txt", "l\n"),
    ] {
        write(&wc, f, body);
    }
    svn(
        &wc,
        &[
            "add", "-q", "a.txt", "b.txt", "c.txt", "d", "gone.txt", "mv.txt", "cl.txt",
        ],
    );
    svn(&wc, &["propset", "-q", "k", "v", "c.txt"]);
    svn(&wc, &["commit", "-q", "-m", "base"]);
    svn(&wc, &["update", "-q"]);

    write(&wc, "a.txt", "changed\n"); // M
    svn(&wc, &["propset", "-q", "k", "x", "c.txt"]); // props modified
    svn(&wc, &["delete", "-q", "b.txt"]); // D
    fs::remove_file(wc.join("gone.txt")).expect("rm"); // missing
    write(&wc, "new.txt", "n\n");
    svn(&wc, &["add", "-q", "new.txt"]); // A
    svn(&wc, &["copy", "-q", "c.txt", "copy.txt"]); // A +
    svn(&wc, &["move", "-q", "mv.txt", "moved.txt"]); // D (moved-to) and A + (moved-from)
    svn(&wc, &["changelist", "-q", "cl1", "cl.txt"]); // unmodified, in a changelist
    write(&wc, "unversioned.txt", "u\n"); // ?
    write(&wc, "junk.o", "o\n"); // I (default global-ignores)
    write(&wc, "d/sub/inner.txt", "i\n"); // ? (the directory is reported once)
    assert_svn_reports(&wc, "mv.txt", "deleted", |r| r.moved_to.is_some());
    assert_svn_reports(&wc, "junk.o", "ignored", |_| true);
    assert_status_matches(&wc);
}

#[test]
fn status_matches_svn_for_replaced_nodes() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (_, wc) = trunk_wc(base.path());
    // R: deleted and added again.
    svn(&wc, &["delete", "-q", "a.txt"]);
    write(&wc, "a.txt", "new a\n");
    svn(&wc, &["add", "-q", "a.txt"]);
    // R +: replaced by a copy.
    svn(&wc, &["delete", "-q", "b.txt"]);
    svn(&wc, &["copy", "-q", "c.txt", "b.txt"]);
    // A replaced directory: the new children are added, the old ones stay deleted.
    svn(&wc, &["delete", "-q", "d"]);
    write(&wc, "d/new.txt", "x\n");
    svn(&wc, &["add", "-q", "d"]);
    assert_svn_reports(&wc, "a.txt", "replaced", |r| !r.copied);
    assert_svn_reports(&wc, "b.txt", "replaced", |r| r.copied);
    assert_svn_reports(&wc, "d", "replaced", |_| true);
    assert_status_matches(&wc);
}

#[test]
fn status_matches_svn_for_missing_and_obstructed() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (_, wc) = trunk_wc(base.path());
    // A missing directory; its children are not reported.
    fs::remove_dir_all(wc.join("d/f")).expect("rm dir");
    // A versioned file replaced by a directory on disk.
    fs::remove_file(wc.join("a.txt")).expect("rm");
    fs::create_dir(wc.join("a.txt")).expect("mkdir");
    assert_svn_reports(&wc, "d/f", "missing", |_| true);
    assert_svn_reports(&wc, "a.txt", "obstructed", |_| true);
    assert_status_matches(&wc);
    // A versioned directory replaced by a file on disk.
    fs::remove_dir_all(wc.join("d")).expect("rm d");
    write(&wc, "d", "not a dir\n");
    assert_svn_reports(&wc, "d", "obstructed", |_| true);
    assert_status_matches(&wc);
}

#[test]
fn status_matches_svn_for_conflicts() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (url, wc) = trunk_wc(base.path());
    let other = checkout(base.path(), &format!("{url}/trunk"), "other", &[]);
    write(&other, "a.txt", "theirs\n");
    svn(&other, &["propset", "-q", "k", "theirs", "c.txt"]);
    write(&other, "b.txt", "theirs b\n");
    svn(&other, &["delete", "-q", "d/e.txt"]);
    svn(&other, &["commit", "-q", "-m", "theirs"]);

    write(&wc, "a.txt", "mine\n"); // text conflict
    svn(&wc, &["propset", "-q", "k", "mine", "c.txt"]); // property conflict
    write(&wc, "d/e.txt", "edited\n"); // tree conflict: local edit, incoming delete
    svn(&wc, &["delete", "-q", "b.txt"]); // tree conflict: local delete, incoming edit
    svn(&wc, &["update", "-q", "--accept", "postpone"]);
    assert_svn_reports(&wc, "a.txt", "conflicted", |_| true);
    assert_svn_reports(&wc, "c.txt", "normal", |r| r.props == "conflicted");
    assert_svn_reports(&wc, "d/e.txt", "added", |r| r.tree_conflicted);
    assert_svn_reports(&wc, "b.txt", "deleted", |r| r.tree_conflicted);
    assert_status_matches(&wc);

    // Marker files removed by hand: the text conflict is no longer reported.
    for entry in fs::read_dir(&wc).expect("ls") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if name.starts_with("a.txt.") {
            fs::remove_file(path).expect("rm marker");
        }
    }
    assert_svn_reports(&wc, "a.txt", "modified", |_| true);
    assert_status_matches(&wc);
}

#[test]
fn status_matches_svn_for_externals_ignored() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (url, wc) = trunk_wc(base.path());
    let seed = base.path().join("seed");
    write(&seed, "lib/l.txt", "l\n");
    svn(&seed, &["add", "-q", "lib"]);
    svn(&seed, &["commit", "-q", "-m", "add lib"]);
    svn(
        &wc,
        &[
            "propset",
            "-q",
            "svn:externals",
            &format!("{url}/lib ext\n^/lib/l.txt fext.txt\n"),
            "d",
        ],
    );
    svn(&wc, &["commit", "-q", "-m", "externals"]);
    svn(&wc, &["update", "-q"]);
    assert!(wc.join("d/ext/l.txt").is_file() && wc.join("d/fext.txt").is_file());
    assert_svn_reports(&wc, "d/ext", "external", |_| true);
    assert_svn_reports(&wc, "d/fext.txt", "normal", |r| r.file_external);
    assert_status_matches(&wc);
    // A modified file external, and a modified file inside the directory external (not ours
    // to report: the external is its own working copy).
    write(&wc, "d/fext.txt", "changed\n");
    write(&wc, "d/ext/l.txt", "changed\n");
    assert_status_matches(&wc);
}

#[test]
fn status_matches_svn_for_switched_nodes() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (url, wc) = trunk_wc(base.path());
    svn(
        base.path(),
        &[
            "copy",
            "-q",
            "-m",
            "branch",
            &format!("{url}/trunk"),
            &format!("{url}/branches/br"),
        ],
    );
    svn(&wc, &["update", "-q"]);
    svn(&wc, &["switch", "-q", &format!("{url}/branches/br/d"), "d"]);
    svn(
        &wc,
        &["switch", "-q", &format!("{url}/branches/br/c.txt"), "c.txt"],
    );
    assert_svn_reports(&wc, "d", "normal", |r| r.switched);
    assert_svn_reports(&wc, "c.txt", "normal", |r| r.switched);
    assert_svn_reports(&wc, "d/e.txt", "normal", |r| !r.switched);
    assert_status_matches(&wc);
    write(&wc, "d/e.txt", "changed on branch\n");
    assert_status_matches(&wc);
}

#[test]
fn status_matches_svn_for_depth_limited_working_copies() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (url, _) = trunk_wc(base.path());
    // Sparse checkout: only the immediate children of trunk.
    let sparse = checkout(
        base.path(),
        &format!("{url}/trunk"),
        "sparse",
        &["--depth", "immediates"],
    );
    write(&sparse, "d/unversioned.txt", "u\n");
    assert_status_matches(&sparse);

    // Excluded and depth-empty subtrees in a full checkout.
    let full = checkout(base.path(), &format!("{url}/trunk"), "full", &[]);
    svn(&full, &["update", "-q", "--set-depth", "exclude", "d/f"]);
    svn(&full, &["update", "-q", "--set-depth", "empty", "d"]);
    write(&full, "d/f/stray.txt", "s\n");
    assert_svn_reports(&full, "d/f", "unversioned", |_| true);
    assert_status_matches(&full);

    // Depth files: subdirectories are absent, files are tracked.
    let files = checkout(
        base.path(),
        &format!("{url}/trunk"),
        "files",
        &["--depth", "files"],
    );
    write(&files, "a.txt", "changed\n");
    fs::create_dir(files.join("d")).expect("mkdir");
    assert_status_matches(&files);
}
