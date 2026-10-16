//! `status` and `diff` compared with what the real svn 1.15 prints for the same working copy.
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{DiffOptions, StatusOptions, checkout, diff, status, update};
use svn_types::{CancelToken, Depth, NodeStatus, PropStatus, WcStatus};

fn letters(s: &WcStatus) -> String {
    let c0 = match s.text {
        NodeStatus::Added => 'A',
        NodeStatus::Deleted => 'D',
        NodeStatus::Modified => 'M',
        NodeStatus::Replaced => 'R',
        NodeStatus::Missing | NodeStatus::Incomplete => '!',
        NodeStatus::Obstructed => '~',
        NodeStatus::Unversioned => '?',
        NodeStatus::Ignored => 'I',
        NodeStatus::Conflicted => 'C',
        _ => ' ',
    };
    let c1 = match s.props {
        PropStatus::Modified => 'M',
        PropStatus::Conflicted => 'C',
        _ => ' ',
    };
    let c6 = if s.tree_conflict { 'C' } else { ' ' };
    let path = if s.path.is_root() {
        ".".to_owned()
    } else {
        s.path.to_string()
    };
    format!("{c0}{c1}{c6} {path}")
}

/// `svn status` lines reduced to text, property and tree-conflict columns plus the path.
fn svn_letters(oracle: &opensvn_svn_oracle::Oracle, wc: &Path) -> Vec<String> {
    svn_letters_with(oracle, wc, &[])
}

fn svn_letters_with(oracle: &opensvn_svn_oracle::Oracle, wc: &Path, extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = svn_status(oracle, wc, extra)
        .lines()
        .filter(|l| {
            // Tree-conflict detail lines, the summary, and the changelist group headers
            // (`--- Changelist 'name':` preceded by an empty line) carry no item status.
            !l.trim_start().starts_with('>')
                && !l.starts_with("Summary")
                && !l.starts_with("  ")
                && !l.starts_with("--- Changelist")
                && !l.is_empty()
        })
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            format!("{}{}{} {}", c[0], c[1], c[6], &l[8..])
        })
        .collect();
    v.sort();
    v
}

async fn our_letters(wc: &Path) -> Vec<String> {
    our_letters_at(wc, Depth::Infinity).await
}

async fn our_letters_at(wc: &Path, depth: Depth) -> Vec<String> {
    let opts = StatusOptions {
        depth,
        ..StatusOptions::default()
    };
    let mut v: Vec<String> = status(&ctx(), wc, &opts, &CancelToken::new())
        .await
        .expect("status")
        .iter()
        .filter(|s| {
            !s.path.is_root()
                || s.text != NodeStatus::Normal
                || matches!(s.props, PropStatus::Modified | PropStatus::Conflicted)
                || s.tree_conflict
        })
        .map(letters)
        .collect();
    v.sort();
    v
}

/// Working copy at r1 made by this client with a representative mix of local changes made by svn.
async fn local_changes() -> Option<(
    opensvn_svn_oracle::Oracle,
    Repo,
    Sandbox,
    std::path::PathBuf,
)> {
    let oracle = oracle()?;
    let repo = Repo::new(&oracle);
    build_r1(&repo);
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(
        &ctx(),
        &repo.fake(None),
        &ours,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    write(&ours.join("README"), "hello\nthere\nworld\n");
    write(
        &ours.join("docs/guide.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
    );
    repo.svn(&ours, &["propset", "-q", "color", "green", "docs"]);
    repo.svn(&ours, &["propset", "-q", "extra", "1", "README"]);
    write(&ours.join("added.txt"), "new\nfile\n");
    write(&ours.join("emptyadd.txt"), "");
    repo.svn(&ours, &["add", "-q", "added.txt", "emptyadd.txt"]);
    repo.svn(&ours, &["rm", "-q", "run.sh"]);
    std::fs::remove_file(ours.join("empty.txt")).expect("rm");
    write(&ours.join("scratch.txt"), "unversioned\n");
    write(&ours.join("build.o"), "ignored by default\n");
    repo.svn(&ours, &["propset", "-q", "svn:ignore", "*.tmp", "."]);
    write(&ours.join("x.tmp"), "ignored by property\n");
    repo.svn(&ours, &["changelist", "-q", "work", "README"]);
    Some((oracle, repo, sb, ours))
}

#[tokio::test(flavor = "multi_thread")]
async fn local_status_matches_svn_status() {
    let Some((oracle, _repo, _sb, ours)) = local_changes().await else {
        return;
    };
    assert_eq!(our_letters(&ours).await, svn_letters(&oracle, &ours));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_changelist_and_ignored_items() {
    let Some((_o, _repo, _sb, ours)) = local_changes().await else {
        return;
    };
    let all = status(
        &ctx(),
        &ours,
        &StatusOptions {
            include_ignored: true,
            ..StatusOptions::default()
        },
        &CancelToken::new(),
    )
    .await
    .expect("status");
    let find = |p: &str| {
        all.iter()
            .find(|s| s.path.as_str() == p)
            .unwrap_or_else(|| panic!("{p} missing"))
    };
    assert_eq!(find("README").changelist.as_deref(), Some("work"));
    assert_eq!(find("build.o").text, NodeStatus::Ignored);
    assert_eq!(find("x.tmp").text, NodeStatus::Ignored);
    assert_eq!(find("scratch.txt").text, NodeStatus::Unversioned);
    assert_eq!(find("empty.txt").text, NodeStatus::Missing);
}

#[tokio::test(flavor = "multi_thread")]
async fn diff_matches_svn_diff_byte_for_byte() {
    let Some((_o, repo, _sb, ours)) = local_changes().await else {
        return;
    };
    let expected = repo.svn(&ours, &["diff"]);
    let got = diff(&ctx(), &ours, &DiffOptions::default(), &CancelToken::new())
        .await
        .expect("diff");
    assert_eq!(String::from_utf8_lossy(&got), expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_diff_after_conflicting_update_match_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(
        &ctx(),
        &repo.fake(Some(1)),
        &ours,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    write(&ours.join("README"), "hello\nmine\n");
    write(
        &ours.join("docs/guide.txt"),
        "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\n",
    );
    write(&seed.join("README"), "hello\ntheirs\n");
    write(
        &seed.join("docs/guide.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
    );
    repo.svn(&seed, &["rm", "-q", "empty.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    write(&ours.join("empty.txt"), "edited\n");
    update(
        &ctx(),
        &repo.fake(None),
        &ours,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update");
    assert_eq!(our_letters(&ours).await, svn_letters(&oracle, &ours));
    let expected = repo.svn(&ours, &["diff"]);
    let got = diff(&ctx(), &ours, &DiffOptions::default(), &CancelToken::new())
        .await
        .expect("diff");
    assert_eq!(String::from_utf8_lossy(&got), expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn status_depth_matches_svn_status_depth() {
    let Some((oracle, _repo, _sb, ours)) = local_changes().await else {
        return;
    };
    write(&ours.join("newdir/inner.txt"), "unversioned dir\n");
    for (depth, word) in [
        (Depth::Empty, "empty"),
        (Depth::Files, "files"),
        (Depth::Immediates, "immediates"),
        (Depth::Infinity, "infinity"),
    ] {
        assert_eq!(
            our_letters_at(&ours, depth).await,
            svn_letters_with(&oracle, &ours, &["--depth", word]),
            "--depth {word}"
        );
    }
}

/// Working copy of `trunk` checked out by svn with a directory external (`ext`), an old-format
/// external with a space in its name, an external two unversioned directories deep
/// (`deep/x/ext`), two file externals (`fext` modified, `fext2` not) and a separate checkout
/// nested inside it (`nested`); local edits inside the externals and the nested checkout.
fn externals_wc(repo: &Repo) -> std::path::PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("lib/a.txt"), "a\n");
    write(&seed.join("lib/sub/b.txt"), "b\n");
    write(&seed.join("trunk/keep.txt"), "k\n");
    repo.svn(&seed, &["add", "-q", "lib", "trunk"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    let defs = repo.base().join("externals.txt");
    write(
        &defs,
        &format!(
            "^/lib ext\n\"old style\" -r1 {}/lib\n-r 1 ^/lib deep/x/ext\n^/lib/a.txt fext\n\
             ^/lib/sub/b.txt fext2\n",
            repo.url
        ),
    );
    repo.svn(
        &seed,
        &[
            "propset",
            "-q",
            "svn:externals",
            "-F",
            defs.to_str().expect("utf8"),
            "trunk",
        ],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    let wc = repo.base().join("wc");
    repo.svn(
        &repo.base(),
        &["checkout", "-q", &format!("{}/trunk", repo.url), "wc"],
    );
    repo.svn(
        &wc,
        &["checkout", "-q", &format!("{}/lib", repo.url), "nested"],
    );
    write(&wc.join("ext/a.txt"), "edited inside the external\n");
    write(
        &wc.join("nested/a.txt"),
        "edited inside the nested checkout\n",
    );
    write(&wc.join("fext"), "edited file external\n");
    write(
        &wc.join("deep/stray.txt"),
        "inside a directory leading to an external\n",
    );
    write(&wc.join("junk.txt"), "unversioned\n");
    wc
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_externals_and_nested_checkouts_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let wc = externals_wc(&repo);
    // Text, property, switched/file-external and tree-conflict columns plus the path.
    let mut theirs: Vec<String> = svn_status(&oracle, &wc, &["--ignore-externals"])
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            format!("{}{}{}{} {}", c[0], c[1], c[4], c[6], &l[8..])
        })
        .collect();
    theirs.sort();
    let mut ours: Vec<String> = status(&ctx(), &wc, &StatusOptions::default(), &CancelToken::new())
        .await
        .expect("status")
        .iter()
        .filter(|s| !s.path.is_root() || s.text != NodeStatus::Normal)
        .map(|s| {
            let c0 = match s.text {
                NodeStatus::External => 'X',
                NodeStatus::Unversioned => '?',
                NodeStatus::Modified => 'M',
                _ => ' ',
            };
            let c1 = if s.props == PropStatus::Modified {
                'M'
            } else {
                ' '
            };
            let c4 = if s.file_external {
                'X'
            } else if s.switched {
                'S'
            } else {
                ' '
            };
            let c6 = if s.tree_conflict { 'C' } else { ' ' };
            format!("{c0}{c1}{c4}{c6} {}", s.path)
        })
        .collect();
    ours.sort();
    assert_eq!(ours, theirs);
    assert_eq!(
        theirs,
        [
            "  X  fext2",
            "M X  fext",
            "?    junk.txt",
            "?    nested",
            "X    deep",
            "X    ext",
            "X    old style",
        ]
        .map(str::to_owned)
        .to_vec()
        .tap_sorted(),
        "the fixture covers every case"
    );
}

trait TapSorted {
    fn tap_sorted(self) -> Self;
}

impl TapSorted for Vec<String> {
    fn tap_sorted(mut self) -> Self {
        self.sort();
        self
    }
}
