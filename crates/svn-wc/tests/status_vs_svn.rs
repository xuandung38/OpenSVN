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
    svn_report_of(wc, "", verbose)
}

/// `svn status --xml` of `target` (a relpath, `""` for the root).
fn svn_report_of(wc: &Path, target: &str, verbose: bool) -> Report {
    let mut args = vec!["status", "--xml", "--no-ignore", "--ignore-externals"];
    if verbose {
        args.push("-v");
    }
    if !target.is_empty() {
        args.push(target);
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

/// Our report of `target` (a relpath, `""` for the root), rendered the way `svn status --xml`
/// prints it (`print_status_xml`).
fn our_report_of(wc: &Path, target: &str, verbose: bool) -> Report {
    let db = WcDb::open(wc).expect("open");
    let target = if target.is_empty() {
        RelPath::root()
    } else {
        RelPath::new(target).expect("relpath")
    };
    let source = DbPristineSource(&db);
    let opts = StatusOptions {
        no_ignore: true,
        get_all: verbose,
        ..StatusOptions::default()
    };
    status(&db, &source, &target, &opts, &CancelToken::new())
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
    assert_status_matches_at(wc, "");
}

/// [`assert_status_matches`] for a status call on `target` (a relpath, `""` for the root).
fn assert_status_matches_at(wc: &Path, target: &str) {
    for verbose in [false, true] {
        let expected = svn_report_of(wc, target, verbose);
        let got = our_report_of(wc, target, verbose);
        if expected != got {
            let mut diff = String::new();
            let keys: BTreeSet<_> = expected.keys().chain(got.keys()).collect();
            for k in keys {
                let (e, g) = (expected.get(k), got.get(k));
                if e != g {
                    diff.push_str(&format!("{k}\n  svn:  {e:?}\n  ours: {g:?}\n"));
                }
            }
            panic!("status mismatch (target = {target:?}, verbose = {verbose}):\n{diff}");
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

/// Regression: `svn:special` inside a property value (or a longer property name) must not make a
/// regular file look like a symlink (it was reported `obstructed`).
#[test]
fn status_matches_svn_for_svn_special_text_in_other_properties() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (_, wc) = trunk_wc(base.path());
    write(&wc, "note.txt", "n\n");
    write(&wc, "named.txt", "m\n");
    svn(&wc, &["add", "-q", "note.txt", "named.txt"]);
    svn(
        &wc,
        &[
            "propset",
            "-q",
            "comment",
            "this is like svn:special but not",
            "note.txt",
        ],
    );
    svn(
        &wc,
        &["propset", "-q", "my:svn:special:x", "*", "named.txt"],
    );
    svn(&wc, &["commit", "-q", "-m", "props"]);
    svn(&wc, &["update", "-q"]);
    assert_svn_reports(&wc, "note.txt", "normal", |r| r.props == "normal");
    assert_svn_reports(&wc, "named.txt", "normal", |r| r.props == "normal");
    assert_status_matches(&wc);
    // Content compare path as well (the stat cache no longer matches).
    for f in ["note.txt", "named.txt"] {
        let body = fs::read(wc.join(f)).expect("read");
        fs::write(wc.join(f), body).expect("rewrite");
    }
    assert_status_matches(&wc);
}

/// Translated files (keywords, eol-style), same-size edits, files whose stat cache misses, a
/// symlink retargeted and then replaced by a regular file, and status on a subdirectory target.
#[test]
fn status_matches_svn_for_translation_symlinks_and_subdir_target() {
    if skip_without_svn() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let (_, wc) = trunk_wc(base.path());
    write(&wc, "kw_id.txt", "$Id$\nbody\n");
    write(&wc, "kw_rev.txt", "$Rev$\nbody\n");
    write(&wc, "crlf.txt", "one\ntwo\n");
    write(&wc, "crlf_lf.txt", "one\ntwo\n");
    write(&wc, "native.txt", "one\ntwo\n");
    write(&wc, "same.txt", "aaaa\n");
    write(&wc, "d/sub.txt", "s\n");
    let mut added = vec![
        "kw_id.txt",
        "kw_rev.txt",
        "crlf.txt",
        "crlf_lf.txt",
        "native.txt",
        "same.txt",
        "d/sub.txt",
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a.txt", wc.join("link")).expect("symlink");
        std::os::unix::fs::symlink("b.txt", wc.join("link2")).expect("symlink");
        added.extend(["link", "link2"]);
    }
    let mut args = vec!["add", "-q"];
    args.extend(&added);
    svn(&wc, &args);
    svn(&wc, &["propset", "-q", "svn:keywords", "Id", "kw_id.txt"]);
    svn(&wc, &["propset", "-q", "svn:keywords", "Rev", "kw_rev.txt"]);
    svn(
        &wc,
        &[
            "propset",
            "-q",
            "svn:eol-style",
            "CRLF",
            "crlf.txt",
            "crlf_lf.txt",
        ],
    );
    svn(
        &wc,
        &["propset", "-q", "svn:eol-style", "native", "native.txt"],
    );
    svn(&wc, &["commit", "-q", "-m", "translated"]);
    svn(&wc, &["update", "-q"]);
    let expanded = fs::read_to_string(wc.join("kw_id.txt")).expect("read");
    assert!(
        expanded.starts_with("$Id: "),
        "keyword not expanded: {expanded}"
    );
    assert_status_matches(&wc);

    // Touch every versioned file (same content, new mtime) so the content compare runs.
    for f in [
        "a.txt",
        "b.txt",
        "c.txt",
        "kw_id.txt",
        "kw_rev.txt",
        "crlf.txt",
        "native.txt",
    ] {
        let body = fs::read(wc.join(f)).expect("read");
        fs::write(wc.join(f), body).expect("rewrite");
    }
    // A CRLF file rewritten with LF: svn repairs the line endings, so it is not modified.
    write(&wc, "crlf_lf.txt", "one\ntwo\n");
    // Same size, different content.
    write(&wc, "same.txt", "bbbb\n");
    // Keyword file with a body edit.
    let rev = fs::read_to_string(wc.join("kw_rev.txt")).expect("read");
    write(&wc, "kw_rev.txt", &rev.replace("body", "edit"));
    write(&wc, "d/sub.txt", "changed\n");
    assert_svn_reports(&wc, "crlf_lf.txt", "normal", |_| true);
    assert_svn_reports(&wc, "same.txt", "modified", |_| true);
    assert_svn_reports(&wc, "kw_rev.txt", "modified", |_| true);
    assert_svn_reports(&wc, "kw_id.txt", "normal", |_| true);
    #[cfg(unix)]
    {
        // Retargeted symlink, and a symlink replaced by a regular file.
        fs::remove_file(wc.join("link")).expect("rm link");
        std::os::unix::fs::symlink("c.txt", wc.join("link")).expect("symlink");
        fs::remove_file(wc.join("link2")).expect("rm link2");
        write(&wc, "link2", "link b.txt");
        assert_svn_reports(&wc, "link", "modified", |_| true);
        assert_svn_reports(&wc, "link2", "obstructed", |_| true);
    }
    assert_status_matches(&wc);
    assert_status_matches_at(&wc, "d");
    assert_status_matches_at(&wc, "d/f");
}

/// Format 32 with `--store-pristine=no`: no pristine texts on disk. A file that misses the stat
/// fast path is compared by checksum (svn 1.15) instead of failing the whole status.
#[test]
fn status_matches_svn_without_stored_pristines() {
    if skip_without_svn_1_15() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp");
    let url = create_repo(base.path());
    let seed = checkout(base.path(), &url, "seed", &[]);
    for (f, body) in [
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
        ("same.txt", "aaaa\n"),
        ("crlf.txt", "one\ntwo\n"),
        ("kw.txt", "$Id$\nbody\n"),
    ] {
        write(&seed, f, body);
    }
    svn(
        &seed,
        &[
            "add", "-q", "a.txt", "b.txt", "same.txt", "crlf.txt", "kw.txt",
        ],
    );
    svn(
        &seed,
        &["propset", "-q", "svn:eol-style", "CRLF", "crlf.txt"],
    );
    svn(&seed, &["propset", "-q", "svn:keywords", "Id", "kw.txt"]);
    svn(&seed, &["commit", "-q", "-m", "seed"]);
    let wc = checkout(
        base.path(),
        &url,
        "wc",
        &["--compatible-version=1.15", "--store-pristine=no"],
    );
    {
        let db = WcDb::open(&wc).expect("open");
        assert_eq!(db.format(), 32);
        assert!(!db.store_pristine());
    }
    let pristine_dir = wc.join(".svn/pristine");
    let stored = fs::read_dir(&pristine_dir)
        .map(|d| {
            d.flatten().any(|e| {
                e.path().is_dir() && fs::read_dir(e.path()).is_ok_and(|mut f| f.next().is_some())
            })
        })
        .unwrap_or(false);
    assert!(!stored, "scenario setup: pristine texts are stored");
    assert_status_matches(&wc);

    // Touched but unchanged, CRLF rewritten with LF, keyword file touched: not modified.
    for f in ["a.txt", "kw.txt"] {
        let body = fs::read(wc.join(f)).expect("read");
        fs::write(wc.join(f), body).expect("rewrite");
    }
    write(&wc, "crlf.txt", "one\ntwo\n");
    // Different content of the same size, and of another size.
    write(&wc, "same.txt", "bbbb\n");
    write(&wc, "b.txt", "bigger\n");
    assert_svn_reports(&wc, "a.txt", "normal", |_| true);
    assert_svn_reports(&wc, "crlf.txt", "normal", |_| true);
    assert_svn_reports(&wc, "same.txt", "modified", |_| true);
    assert_status_matches(&wc);
}
