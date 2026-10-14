//! Local status of obstructed nodes compared with svn 1.15: a versioned directory replaced on
//! disk by a file or by a symlink, and `svn:special` links and regular files swapped for each
//! other. svn reports `~` for the node and goes on with the rest of the working copy.
//!
//! One deliberate difference: svn follows a symlink that replaced a versioned directory (out of
//! the working copy if it points there) and reports what it finds behind it. We never follow
//! it: the versioned children below it are missing (`!`), as below a directory replaced by a
//! file, and nothing behind it is listed.
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{StatusOptions, status};
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
    let path = if s.path.is_root() {
        ".".to_owned()
    } else {
        s.path.to_string()
    };
    format!("{c0}{c1} {path}")
}

/// `svn status <extra>` run in `wc`, as sorted `XY path` lines.
fn svn_letters(oracle: &opensvn_svn_oracle::Oracle, wc: &Path, extra: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = svn_status(oracle, wc, extra)
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with("  ") && !l.trim_start().starts_with('>'))
        .map(|l| {
            let c: Vec<char> = l.chars().collect();
            format!("{}{} {}", c[0], c[1], &l[8..])
        })
        .collect();
    v.sort();
    v
}

/// Our status of `target` (below `wc`), as sorted `XY path` lines.
async fn our_letters(wc: &Path, target: &str, opts: &StatusOptions) -> Vec<String> {
    let path = if target.is_empty() {
        wc.to_path_buf()
    } else {
        wc.join(target)
    };
    let mut v: Vec<String> = status(&ctx(), &path, opts, &CancelToken::new())
        .await
        .expect("status")
        .iter()
        .filter(|s| {
            !s.path.is_root()
                || s.text != NodeStatus::Normal
                || matches!(s.props, PropStatus::Modified | PropStatus::Conflicted)
        })
        .map(letters)
        .collect();
    v.sort();
    v
}

fn option_sets() -> Vec<(StatusOptions, Vec<&'static str>)> {
    vec![
        (StatusOptions::default(), vec![]),
        (
            StatusOptions {
                include_ignored: true,
                ..StatusOptions::default()
            },
            vec!["--no-ignore"],
        ),
        (
            StatusOptions {
                depth: Depth::Immediates,
                ..StatusOptions::default()
            },
            vec!["--depth", "immediates"],
        ),
    ]
}

/// r1: directories with files and subdirectories, enough of them for the listing to run on
/// several threads.
fn build_dirs(repo: &Repo) {
    let seed = repo.checkout("seed");
    for d in ["filed", "linked", "nested", "plain", "other"] {
        write(&seed.join(format!("{d}/a.txt")), &format!("{d} a\n"));
        write(&seed.join(format!("{d}/sub/b.txt")), &format!("{d} b\n"));
        write(
            &seed.join(format!("{d}/sub/deep/c.txt")),
            &format!("{d} c\n"),
        );
    }
    write(&seed.join("top.txt"), "top\n");
    let all = [
        "add", "-q", "filed", "linked", "nested", "plain", "other", "top.txt",
    ];
    repo.svn(&seed, &all);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
}

/// Replaces versioned directories on disk: `filed` by a file, `nested/sub` by a file,
/// `linked` by a symlink to a copy of it outside the working copy, with an edit and an
/// unversioned file there that must not show up.
#[cfg(unix)]
fn obstruct_dirs(wc: &Path, outside: &Path) {
    fs::remove_dir_all(wc.join("filed")).expect("rm filed");
    fs::write(wc.join("filed"), "now a file\n").expect("file over dir");
    fs::remove_dir_all(wc.join("nested/sub")).expect("rm nested/sub");
    fs::write(wc.join("nested/sub"), "now a file\n").expect("file over subdir");
    fs::rename(wc.join("linked"), outside).expect("move linked out");
    // Behind the link: an edit and an unversioned file.
    fs::write(outside.join("a.txt"), "edited behind the link\n").expect("edit");
    fs::write(outside.join("stray.txt"), "unversioned behind the link\n").expect("stray");
    std::os::unix::fs::symlink(outside, wc.join("linked")).expect("symlink");
    // Ordinary changes elsewhere still show.
    fs::write(wc.join("plain/a.txt"), "modified\n").expect("modify");
    fs::write(wc.join("other/new.txt"), "new\n").expect("unversioned");
}

/// What we report for `linked`, the versioned directory replaced by a symlink: obstructed, its
/// versioned children missing (svn follows the link instead).
fn linked_lines(depth: Depth) -> Vec<String> {
    let mut v = vec!["~  linked".to_owned()];
    if depth == Depth::Infinity {
        for p in [
            "linked/a.txt",
            "linked/sub",
            "linked/sub/b.txt",
            "linked/sub/deep",
            "linked/sub/deep/c.txt",
        ] {
            v.push(format!("!  {p}"));
        }
    }
    v.sort();
    v
}

fn under_linked(l: &str) -> bool {
    l[3..] == *"linked" || l[3..].starts_with("linked/")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn directory_replaced_by_file_or_symlink_is_obstructed_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_dirs(&repo);
    let wc = repo.checkout("wc");
    let outside = repo.base().join("outside");
    obstruct_dirs(&wc, &outside);

    for (opts, extra) in option_sets() {
        let ours = our_letters(&wc, "", &opts).await;
        let svn = svn_letters(&oracle, &wc, &extra);
        // Everything but the symlinked directory is exactly svn's.
        let keep = |v: &[String]| -> Vec<String> {
            v.iter().filter(|l| !under_linked(l)).cloned().collect()
        };
        assert_eq!(keep(&ours), keep(&svn), "status {extra:?}");
        // The symlinked directory: obstructed, children missing, nothing behind the link.
        let mut mine: Vec<String> = ours.iter().filter(|l| under_linked(l)).cloned().collect();
        mine.sort();
        let mut want = linked_lines(opts.depth);
        want.sort();
        assert_eq!(mine, want, "status {extra:?}");
        assert!(
            svn.iter().any(|l| l == "~  linked"),
            "svn reports the symlinked directory as obstructed: {svn:?}"
        );
    }

    // The fixture covers what it claims (default options, as svn reports it).
    let svn = svn_letters(&oracle, &wc, &[]);
    for want in [
        "~  filed",
        "!  filed/a.txt",
        "!  filed/sub/deep/c.txt",
        "~  nested/sub",
        "!  nested/sub/deep",
        "M  plain/a.txt",
        "?  other/new.txt",
    ] {
        assert!(svn.iter().any(|l| l == want), "{want} in {svn:?}");
    }

    // Targets at, below and next to the obstructions.
    for target in [
        "filed",
        "filed/sub",
        "filed/a.txt",
        "nested",
        "nested/sub/deep",
    ] {
        let ours = our_letters(&wc, target, &StatusOptions::default()).await;
        assert_eq!(
            ours,
            svn_letters(&oracle, &wc, &[target]),
            "status {target}"
        );
    }
    let ours = our_letters(&wc, "linked", &StatusOptions::default()).await;
    assert_eq!(ours, linked_lines(Depth::Infinity), "status linked");
    let ours = our_letters(&wc, "linked/sub", &StatusOptions::default()).await;
    assert_eq!(
        ours,
        [
            "!  linked/sub",
            "!  linked/sub/b.txt",
            "!  linked/sub/deep",
            "!  linked/sub/deep/c.txt"
        ]
        .map(str::to_owned)
        .to_vec(),
        "status linked/sub"
    );

    // Nothing was written behind the link.
    assert_eq!(
        fs::read_to_string(outside.join("a.txt")).expect("read"),
        "edited behind the link\n"
    );
    assert!(outside.join("stray.txt").is_file());
}

/// r1: regular files and `svn:special` links.
#[cfg(unix)]
fn build_links(repo: &Repo) {
    let seed = repo.checkout("seed");
    for f in ["reg", "reg2", "regprop", "kept"] {
        write(&seed.join(f), &format!("{f}\n"));
    }
    for (l, t) in [
        ("lnk", "reg"),
        ("lnk2", "reg2"),
        ("lnkprop", "reg"),
        ("lnkkept", "kept"),
    ] {
        std::os::unix::fs::symlink(t, seed.join(l)).expect("symlink");
    }
    repo.svn(
        &seed,
        &[
            "add", "-q", "reg", "reg2", "regprop", "kept", "lnk", "lnk2", "lnkprop", "lnkkept",
        ],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn special_and_regular_files_swapped_are_obstructed_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_links(&repo);
    let wc = repo.checkout("wc");
    // A link replaced by a regular file holding its target (what a copy tool might leave).
    fs::remove_file(wc.join("lnk")).expect("rm lnk");
    fs::write(wc.join("lnk"), "reg").expect("file over link");
    // A link replaced by a regular file holding svn's repository form of it.
    fs::remove_file(wc.join("lnk2")).expect("rm lnk2");
    fs::write(wc.join("lnk2"), "link reg2").expect("file over link");
    // A regular file replaced by a symlink.
    fs::remove_file(wc.join("reg")).expect("rm reg");
    std::os::unix::fs::symlink("reg2", wc.join("reg")).expect("link over file");
    // svn:special set on a regular file, and removed from a link.
    repo.svn(&wc, &["propset", "-q", "svn:special", "*", "regprop"]);
    repo.svn(&wc, &["propdel", "-q", "svn:special", "lnkprop"]);
    // Added link and added file, then swapped.
    std::os::unix::fs::symlink("kept", wc.join("newlink")).expect("symlink");
    write(&wc.join("newfile"), "new\n");
    repo.svn(&wc, &["add", "-q", "newlink", "newfile"]);
    fs::remove_file(wc.join("newlink")).expect("rm newlink");
    fs::write(wc.join("newlink"), "kept").expect("file over added link");
    fs::remove_file(wc.join("newfile")).expect("rm newfile");
    std::os::unix::fs::symlink("kept", wc.join("newfile")).expect("link over added file");
    // A link retargeted stays a text modification.
    fs::remove_file(wc.join("lnkkept")).expect("rm lnkkept");
    std::os::unix::fs::symlink("reg2", wc.join("lnkkept")).expect("retarget");

    for (opts, extra) in option_sets() {
        let ours = our_letters(&wc, "", &opts).await;
        assert_eq!(ours, svn_letters(&oracle, &wc, &extra), "status {extra:?}");
    }
    let svn = svn_letters(&oracle, &wc, &[]);
    for want in ["~  lnk", "~  lnk2", "~  reg", "M  lnkkept"] {
        assert!(svn.iter().any(|l| l == want), "{want} in {svn:?}");
    }
    for target in ["lnk", "reg", "regprop", "lnkprop", "newlink"] {
        let ours = our_letters(&wc, target, &StatusOptions::default()).await;
        assert_eq!(
            ours,
            svn_letters(&oracle, &wc, &[target]),
            "status {target}"
        );
    }
}
