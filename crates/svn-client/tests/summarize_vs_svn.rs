//! `svn diff --summarize` against svn 1.15: two repository locations at chosen revisions, and a
//! working copy with local changes against another location.
//!
//! The repository is made by svn (a branch copied with `svn cp`, then edits on trunk: text,
//! properties on a file and a folder, a deleted folder, an added folder, a move, a replaced file)
//! and replayed into the fake session. svn's answer is read from `--xml` and compared entry by
//! entry (path, item, props, kind); the order of svn's output follows its drive, so both sides are
//! sorted.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{SummaryKind, SummaryOptions, checkout, summarize_repos, summarize_wc};
use svn_types::{CancelToken, Depth, NodeKind, RepoUrl, Revnum};

/// r1: trunk and branches; r2: content; r3: `branches/b` copied from trunk; r4: edits on trunk;
/// r5: an edit on the branch, a property on the trunk root.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    repo.svn(&seed, &["mkdir", "-q", "trunk", "branches"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    let t = seed.join("trunk");
    write(&t.join("d/a"), "a\n");
    write(&t.join("d/sub/b"), "b\n");
    write(&t.join("e/c"), "c\n");
    write(&t.join("e/deep/f"), "f\n");
    write(&t.join("x"), "x\n");
    write(&t.join("y"), "y\n");
    write(&t.join("z"), "z\n");
    write(&t.join("both"), "both\n");
    repo.svn(&t, &["add", "-q", "d", "e", "x", "y", "z", "both"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&seed, &["copy", "-q", "trunk", "branches/b"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r3: branch"]);
    write(&t.join("x"), "x\nmore\n");
    repo.svn(&t, &["propset", "-q", "p", "v", "y"]);
    repo.svn(&t, &["propset", "-q", "dp", "v", "d"]);
    write(&t.join("both"), "both\nchanged\n");
    repo.svn(&t, &["propset", "-q", "q", "1", "both"]);
    repo.svn(&t, &["rm", "-q", "e"]);
    write(&t.join("n/f"), "n\n");
    write(&t.join("n/m/g"), "g\n");
    repo.svn(&t, &["add", "-q", "n"]);
    repo.svn(&t, &["rm", "-q", "z"]);
    write(&t.join("z"), "zz\n");
    repo.svn(&t, &["add", "-q", "z"]);
    write(&t.join("w"), "new\n");
    repo.svn(&t, &["add", "-q", "w"]);
    repo.svn(&t, &["mv", "-q", "d/a", "d/a2"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r4: trunk edits"]);
    write(&seed.join("branches/b/x"), "x\nbranch\n");
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["propset", "-q", "rootprop", "1", "trunk"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r5"]);
}

type Row = (String, &'static str, &'static str, &'static str);

fn attr<'a>(tag: &'a str, name: &str) -> &'a str {
    let key = format!("{name}=\"");
    let start = tag.find(&key).map(|i| i + key.len()).expect(name);
    let end = tag[start..].find('"').expect("quote") + start;
    &tag[start..end]
}

fn item(s: &str) -> &'static str {
    match s {
        "added" => "added",
        "deleted" => "deleted",
        "modified" => "modified",
        "none" => "none",
        other => panic!("item {other}"),
    }
}

fn props(s: &str) -> &'static str {
    match s {
        "none" => "none",
        "modified" => "modified",
        other => panic!("props {other}"),
    }
}

fn kind(s: &str) -> &'static str {
    match s {
        "file" => "file",
        "dir" => "dir",
        other => panic!("kind {other}"),
    }
}

/// svn's `--xml` summary, paths made relative to `base` (a URL or `.`).
fn parse_xml(xml: &str, base: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    // `<paths>` starts with `<path` too.
    for chunk in xml
        .split("<path")
        .skip(1)
        .filter(|c| c.starts_with(char::is_whitespace))
    {
        let close = chunk.find('>').expect("tag end");
        let tag = &chunk[..close];
        let text = &chunk[close + 1..chunk.find("</path>").expect("path end")];
        let path = if text == base || text == "." {
            String::new()
        } else if let Some(rest) = text.strip_prefix(&format!("{base}/")) {
            rest.to_owned()
        } else {
            text.to_owned()
        };
        rows.push((
            path,
            item(attr(tag, "item")),
            props(attr(tag, "props")),
            kind(attr(tag, "kind")),
        ));
    }
    rows.sort();
    rows
}

fn ours(entries: &[svn_client::SummaryEntry]) -> Vec<Row> {
    let mut rows: Vec<Row> = entries
        .iter()
        .map(|e| {
            (
                e.path.as_str().to_owned(),
                match e.kind {
                    SummaryKind::Added => "added",
                    SummaryKind::Deleted => "deleted",
                    SummaryKind::Modified => "modified",
                    SummaryKind::Normal => "none",
                },
                if e.props_changed { "modified" } else { "none" },
                match e.node_kind {
                    NodeKind::Dir => "dir",
                    NodeKind::File => "file",
                    other => panic!("{other:?} for {}", e.path),
                },
            )
        })
        .collect();
    rows.sort();
    rows
}

fn fake_url(rel: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url")
}

fn svn_summary(repo: &Repo, cwd: &Path, args: &[&str], base: &str) -> Vec<Row> {
    let mut all = vec!["diff", "--summarize", "--xml"];
    all.extend_from_slice(args);
    parse_xml(&repo.svn(cwd, &all), base)
}

/// `(left, left_rev, right, right_rev, notice_ancestry)`, repository-relative locations.
const CASES: &[(&str, u64, &str, Option<u64>, bool)] = &[
    // The branch against trunk at HEAD: every kind of change, the root property included.
    ("branches/b", 5, "trunk", None, false),
    // The other way round: additions become deletions and the reverse.
    ("trunk", 5, "branches/b", None, false),
    // Two revisions of the same location.
    ("trunk", 2, "trunk", Some(4), false),
    // Not `--notice-ancestry` (a replaced file is then a deletion and an addition): the fake keeps
    // no node identities, and the app always ignores ancestry like `svn diff`. The replaced file
    // `z` is compared as modified here and over DAV (`compare_summary_matches_svn` in
    // apps/opensvn/src-tauri/tests/real_engine_dav.rs).
    // An older right side than the left one.
    ("trunk", 4, "trunk", Some(3), false),
    // Nothing differs.
    ("trunk", 2, "branches/b", Some(3), false),
];

#[tokio::test]
async fn repository_locations_at_chosen_revisions_match_svn() {
    let Some(oracle) = oracle_1_15() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let fake = repo.fake(None);
    let cancel = CancelToken::new();
    for &(left, left_rev, right, right_rev, notice) in CASES {
        let left_url = format!("{}/{left}", repo.url);
        let right_url = format!(
            "{}/{right}@{}",
            repo.url,
            right_rev.map_or("HEAD".to_owned(), |r| r.to_string())
        );
        let mut args = vec![];
        let left_arg = format!("{left_url}@{left_rev}");
        args.push(left_arg.as_str());
        args.push(right_url.as_str());
        if notice {
            args.push("--notice-ancestry");
        }
        let theirs = svn_summary(&repo, &repo.base(), &args, &left_url);

        let session = fake.at(&fake_url(left)).expect("session");
        let summary = summarize_repos(
            &session,
            Revnum(left_rev),
            &fake_url(right),
            right_rev.map(Revnum),
            SummaryOptions {
                depth: Depth::Infinity,
                ignore_ancestry: !notice,
            },
            &cancel,
        )
        .await
        .expect("summarize");
        assert_eq!(
            ours(&summary.entries),
            theirs,
            "{left}@{left_rev} -> {right}@{right_rev:?} (notice ancestry: {notice})"
        );
        assert_eq!(summary.right_rev, Revnum(right_rev.unwrap_or(5)));
    }
    // Every kind of change is covered by the cases above.
    let first = svn_summary(
        &repo,
        &repo.base(),
        &[
            &format!("{}/branches/b@5", repo.url),
            &format!("{}/trunk", repo.url),
        ],
        &format!("{}/branches/b", repo.url),
    );
    for wanted in [
        ("e/deep/f", "deleted", "none", "file"),
        ("n/m/g", "added", "none", "file"),
        ("both", "modified", "modified", "file"),
        ("y", "none", "modified", "file"),
        ("d", "none", "modified", "dir"),
        ("", "none", "modified", "dir"),
    ] {
        assert!(
            first
                .iter()
                .any(|r| (r.0.as_str(), r.1, r.2, r.3) == wanted),
            "{wanted:?} in {first:?}"
        );
    }
}

/// The working copy of trunk at r4 with local changes, against the branch: what svn reports for
/// `--old=WC --new=URL`, apart from the root, which svn 1.15 marks as having property changes
/// whenever local changes exist (none of its properties differ; `svn diff` prints nothing for it).
#[tokio::test]
async fn a_working_copy_with_local_changes_against_another_location_matches_svn() {
    let Some(oracle) = oracle_1_15() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let fake = repo.fake(None);
    let sb = Sandbox::new();
    let theirs = repo.base().join("svnwc");
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            "-r",
            "4",
            &format!("{}/trunk", repo.url),
            theirs.to_str().unwrap(),
        ],
    );
    let at_trunk = fake.at(&fake_url("trunk")).expect("session");
    let mine = sb.path("wc");
    checkout(
        &ctx(),
        &at_trunk,
        &mine,
        Some(Revnum(4)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    for wc in [&theirs, &mine] {
        // A local edit of a file the branch does not change, and of one it changes.
        write(&wc.join("y"), "y\nlocal\n");
        write(&wc.join("both"), "both\nlocal\n");
        // A local addition, and a deletion of a file the branch does not have either.
        write(&wc.join("q"), "q\n");
        repo.svn(wc, &["add", "-q", "q"]);
        repo.svn(wc, &["rm", "-q", "w"]);
        // A deletion of a file the branch has, a local property on a folder.
        repo.svn(wc, &["rm", "-q", "d/sub/b"]);
        repo.svn(wc, &["propset", "-q", "lp", "1", "n"]);
        // Unversioned files are not part of the comparison.
        write(&wc.join("scratch.txt"), "scratch\n");
    }
    for (right, right_rev) in [
        ("branches/b", None),
        ("branches/b", Some(3)),
        ("trunk", Some(2)),
    ] {
        let right_url = format!(
            "{}/{right}@{}",
            repo.url,
            right_rev.map_or("HEAD".to_owned(), |r: u64| r.to_string())
        );
        let mut svn_rows = svn_summary(
            &repo,
            &theirs,
            &["--old=.", &format!("--new={right_url}")],
            ".",
        );
        svn_rows.retain(|r| !r.0.is_empty());
        let summary = summarize_wc(
            &ctx(),
            &at_trunk,
            &mine,
            &fake_url(right),
            right_rev.map(Revnum),
            SummaryOptions::default(),
            &CancelToken::new(),
        )
        .await
        .expect("summarize");
        let mut rows = ours(&summary.entries);
        rows.retain(|r| !r.0.is_empty());
        assert_eq!(rows, svn_rows, "working copy -> {right}@{right_rev:?}");
    }
}

/// A sparse working copy (folders below the root left at depth empty) against its own location
/// and against the branch: the folders it leaves out are not "added" (svn sends a full depth as
/// `unknown`, so the recorded depths hold). A folder replaced locally by a file, with no change
/// on the repository side, is a deletion and an addition, as in svn. A file replaced by a folder
/// is checked on our side only: svn 1.15 fails on it (E145001, "Can only get the pristine
/// contents of files").
#[tokio::test]
async fn a_sparse_working_copy_with_local_kind_changes_matches_svn() {
    let Some(oracle) = oracle_1_15() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let fake = repo.fake(None);
    let sb = Sandbox::new();
    let theirs = repo.base().join("svnwc-sparse");
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            "-r",
            "4",
            "--depth",
            "immediates",
            &format!("{}/trunk", repo.url),
            theirs.to_str().unwrap(),
        ],
    );
    let at_trunk = fake.at(&fake_url("trunk")).expect("session");
    let mine = sb.path("wc");
    checkout(
        &ctx(),
        &at_trunk,
        &mine,
        Some(Revnum(4)),
        Depth::Immediates,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    for wc in [&theirs, &mine] {
        // svn 1.15 marks a file whose properties alone differ as modified in this mode too
        // ("MM" for `y` against the branch), though `svn diff` shows only the property change;
        // a local edit makes both sides agree that the text differs.
        write(&wc.join("y"), "y\nlocal\n");
        // Folder (at depth empty) -> file.
        repo.svn(wc, &["rm", "-q", "n"]);
        write(&wc.join("n"), "n is a file\n");
        repo.svn(wc, &["add", "-q", "n"]);
    }
    for (right, right_rev) in [("trunk", Some(4)), ("branches/b", None)] {
        let right_url = format!(
            "{}/{right}@{}",
            repo.url,
            right_rev.map_or("HEAD".to_owned(), |r: u64| r.to_string())
        );
        let mut svn_rows = svn_summary(
            &repo,
            &theirs,
            &["--old=.", &format!("--new={right_url}")],
            ".",
        );
        svn_rows.retain(|r| !r.0.is_empty());
        let summary = summarize_wc(
            &ctx(),
            &at_trunk,
            &mine,
            &fake_url(right),
            right_rev.map(Revnum),
            SummaryOptions::default(),
            &CancelToken::new(),
        )
        .await
        .expect("summarize");
        let mut rows = ours(&summary.entries);
        rows.retain(|r| !r.0.is_empty());
        assert_eq!(
            rows, svn_rows,
            "sparse working copy -> {right}@{right_rev:?}"
        );
    }

    // File -> folder, ours only.
    repo.svn(&mine, &["rm", "-q", "--force", "y"]);
    std::fs::create_dir(mine.join("y")).expect("mkdir");
    repo.svn(&mine, &["add", "-q", "y"]);
    let summary = summarize_wc(
        &ctx(),
        &at_trunk,
        &mine,
        &fake_url("trunk"),
        Some(Revnum(4)),
        SummaryOptions::default(),
        &CancelToken::new(),
    )
    .await
    .expect("summarize");
    let rows = ours(&summary.entries);
    for wanted in [
        ("y", "deleted", "none", "dir"),
        ("y", "added", "none", "file"),
    ] {
        assert!(
            rows.iter().any(|r| (r.0.as_str(), r.1, r.2, r.3) == wanted),
            "{wanted:?} in {rows:?}"
        );
    }
}
