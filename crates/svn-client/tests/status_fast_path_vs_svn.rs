//! Local status compared with svn 1.15 where the size+mtime fast path decides: same-size edits
//! with and without the recorded mtime restored, touched files, keyword and eol-style files, on
//! a tree wide enough for the directory listings to run on several threads. Both a working copy
//! checked out by svn and one checked out by this client (each records its own stat cache).
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use common::oracle::*;
use common::*;
use svn_client::{StatusOptions, checkout, status};
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

async fn our_letters(wc: &Path, opts: &StatusOptions) -> Vec<String> {
    let mut v: Vec<String> = status(&ctx(), wc, opts, &CancelToken::new())
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

const DIRS: usize = 12;
const SUBDIRS: usize = 3;
const FILES: usize = 5;

fn body(d: usize, s: usize, f: usize) -> String {
    format!("dir {d} sub {s} file {f}\nsecond line\n")
}

/// r1: a wide tree plus keyword, eol-style and binary files.
fn build_tree(repo: &Repo) {
    let seed = repo.checkout("seed");
    for d in 0..DIRS {
        for s in 0..SUBDIRS {
            for f in 0..FILES {
                write(&seed.join(format!("d{d:02}/s{s}/f{f}.txt")), &body(d, s, f));
            }
        }
    }
    write(&seed.join("kw.txt"), "$Id$\n$Rev$\nkeyword file\n");
    write(&seed.join("native.txt"), "a\nb\nc\n");
    write(&seed.join("crlf.txt"), "a\nb\nc\n");
    fs::write(seed.join("bin.dat"), [0u8, 1, 2, 3, 255, 254, 0, 9]).expect("bin");
    let mut add = vec!["add", "-q", "kw.txt", "native.txt", "crlf.txt", "bin.dat"];
    let dirs: Vec<String> = (0..DIRS).map(|d| format!("d{d:02}")).collect();
    add.extend(dirs.iter().map(String::as_str));
    repo.svn(&seed, &add);
    repo.svn(
        &seed,
        &["propset", "-q", "svn:keywords", "Id Rev", "kw.txt"],
    );
    repo.svn(
        &seed,
        &[
            "propset",
            "-q",
            "svn:eol-style",
            "native",
            "kw.txt",
            "native.txt",
        ],
    );
    repo.svn(
        &seed,
        &["propset", "-q", "svn:eol-style", "CRLF", "crlf.txt"],
    );
    repo.svn(
        &seed,
        &[
            "propset",
            "-q",
            "svn:mime-type",
            "application/octet-stream",
            "bin.dat",
        ],
    );
    repo.svn(&seed, &["propset", "-q", "svn:ignore", "*.log", "d03"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
}

fn mtime(p: &Path) -> SystemTime {
    fs::symlink_metadata(p)
        .expect("stat")
        .modified()
        .expect("mtime")
}

fn set_mtime(p: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(p)
        .expect("open")
        .set_modified(t)
        .expect("set mtime");
}

/// Rewrites `p` with `text` and gives it `t` as mtime.
fn rewrite_at(p: &Path, text: &[u8], t: SystemTime) {
    fs::write(p, text).expect("write");
    set_mtime(p, t);
}

/// Local changes whose detection depends on the stat cache (and the usual other states).
fn mutate(wc: &Path) {
    let later = |p: &Path| mtime(p) + Duration::from_secs(7);
    // Same size, new mtime: content compare finds the change.
    let p = wc.join("d00/s0/f0.txt");
    let t = later(&p);
    rewrite_at(&p, body(9, 9, 9).as_bytes(), t);
    // Same size, the recorded mtime restored: the fast path says unmodified, like svn.
    let p = wc.join("d01/s1/f1.txt");
    let t = mtime(&p);
    rewrite_at(&p, body(8, 8, 8).as_bytes(), t);
    // Same content, new mtime: unmodified after the compare.
    let p = wc.join("d02/s2/f2.txt");
    let t = later(&p);
    rewrite_at(&p, &fs::read(&p).expect("read"), t);
    // Size differs.
    let p = wc.join("d04/s0/f3.txt");
    let mut text = fs::read(&p).expect("read");
    text.extend_from_slice(b"appended\n");
    rewrite_at(&p, &text, later(&p));
    // Keyword file touched, and its expanded keyword value edited (contracts to the same).
    let p = wc.join("kw.txt");
    let t = later(&p);
    rewrite_at(&p, &fs::read(&p).expect("read"), t);
    let text = String::from_utf8(fs::read(&p).expect("read")).expect("utf8");
    let start = text.find("$Rev:").expect("expanded Rev");
    let end = start + 1 + text[start + 1..].find('$').expect("closing $");
    let edited = format!("{}$Rev: 12345 ${}", &text[..start], &text[end + 1..]);
    assert_ne!(edited, text, "keyword expanded: {text:?}");
    rewrite_at(&p, edited.as_bytes(), t + Duration::from_secs(1));
    // eol-style files with other line endings than the working form.
    let p = wc.join("crlf.txt");
    rewrite_at(&p, b"a\nb\nc\n", later(&p));
    let p = wc.join("native.txt");
    rewrite_at(&p, b"a\r\nb\r\nc\r\n", later(&p));
    // Binary file, same size, other bytes.
    let p = wc.join("bin.dat");
    rewrite_at(&p, &[0u8, 1, 2, 3, 255, 254, 0, 8], later(&p));
    // Missing directory, missing file, file obstructed by a directory.
    fs::remove_dir_all(wc.join("d05/s1")).expect("rm dir");
    fs::remove_file(wc.join("d06/s0/f4.txt")).expect("rm file");
    let p = wc.join("d07/s2/f0.txt");
    fs::remove_file(&p).expect("rm");
    fs::create_dir(&p).expect("obstruct");
    // Unversioned and ignored items at several levels.
    write(&wc.join("d08/new.txt"), "new\n");
    write(&wc.join("d09/s1/newdir/inner.txt"), "new\n");
    write(&wc.join("d03/s0/trace.log"), "not ignored here\n");
    write(&wc.join("d03/trace.log"), "ignored by svn:ignore\n");
    write(&wc.join("d10/s2/obj.o"), "ignored by default\n");
    // Many more modified files spread over the tree.
    for d in (0..DIRS).step_by(3) {
        let p = wc.join(format!("d{d:02}/s1/f4.txt"));
        if p.is_file() {
            let t = later(&p);
            rewrite_at(&p, format!("changed {d}\n").as_bytes(), t);
        }
    }
}

async fn assert_matches_svn(oracle: &opensvn_svn_oracle::Oracle, wc: &Path) {
    for (opts, extra) in [
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
    ] {
        // Ours first: svn status never writes the stat cache, so it cannot help us.
        let ours = our_letters(wc, &opts).await;
        assert_eq!(ours, svn_letters(oracle, wc, &extra), "status {extra:?}");
    }
}

fn expected() -> Vec<String> {
    let mut v: Vec<String> = [
        "M  d00/s0/f0.txt",
        "M  d00/s1/f4.txt",
        "M  d03/s1/f4.txt",
        "M  d04/s0/f3.txt",
        "!  d05/s1",
        "!  d05/s1/f0.txt",
        "!  d05/s1/f1.txt",
        "!  d05/s1/f2.txt",
        "!  d05/s1/f3.txt",
        "!  d05/s1/f4.txt",
        "!  d06/s0/f4.txt",
        "M  d06/s1/f4.txt",
        "~  d07/s2/f0.txt",
        "?  d03/s0/trace.log",
        "?  d08/new.txt",
        "?  d09/s1/newdir",
        "M  d09/s1/f4.txt",
        "M  bin.dat",
    ]
    .map(str::to_owned)
    .to_vec();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn fast_path_matches_svn_on_svn_checkout() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_tree(&repo);
    let wc = repo.checkout("wc");
    mutate(&wc);
    assert_matches_svn(&oracle, &wc).await;
    let mut svn_said = svn_letters(&oracle, &wc, &[]);
    // `crlf.txt` / `native.txt` with foreign line endings: whatever svn says, we match it above.
    svn_said.retain(|l| !l.ends_with("crlf.txt") && !l.ends_with("native.txt"));
    assert_eq!(svn_said, expected(), "the fixture covers every case");
}

#[tokio::test(flavor = "multi_thread")]
async fn fast_path_matches_svn_on_our_checkout() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_tree(&repo);
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &repo.fake(None),
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    // A clean checkout is clean for both, with no content compare needed.
    assert_eq!(
        our_letters(&wc, &StatusOptions::default()).await,
        Vec::<String>::new()
    );
    assert_eq!(svn_letters(&oracle, &wc, &[]), Vec::<String>::new());
    mutate(&wc);
    assert_matches_svn(&oracle, &wc).await;
}

/// A versioned directory replaced by a symlink is obstructed and the rest of the working copy
/// is still reported (the whole status used to fail with `UnsafePath`). The link is not followed:
/// what is behind it is never listed or compared, and the versioned children are missing.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn symlinked_versioned_directory_is_obstructed_not_followed() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_tree(&repo);
    let wc = repo.checkout("wc");
    let elsewhere = repo.base().join("elsewhere");
    fs::rename(wc.join("d02"), &elsewhere).expect("move away");
    std::os::unix::fs::symlink(&elsewhere, wc.join("d02")).expect("symlink");
    write(
        &elsewhere.join("behind.txt"),
        "unversioned behind the link\n",
    );
    write(&elsewhere.join("s0/f0.txt"), "edited behind the link\n");
    write(&wc.join("d08/new.txt"), "new\n");
    let got = status(&ctx(), &wc, &StatusOptions::default(), &CancelToken::new())
        .await
        .expect("status");
    let lines: Vec<String> = got.iter().map(letters).collect();
    assert!(lines.contains(&"~  d02".to_owned()), "{lines:?}");
    assert!(lines.contains(&"!  d02/s0/f0.txt".to_owned()), "{lines:?}");
    assert!(lines.contains(&"?  d08/new.txt".to_owned()), "{lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("behind")),
        "listed through the link: {lines:?}"
    );
    let below: Vec<&WcStatus> = got
        .iter()
        .filter(|s| s.path.as_str().starts_with("d02/"))
        .collect();
    assert_eq!(below.len(), SUBDIRS * (FILES + 1), "{lines:?}");
    assert!(
        below.iter().all(|s| s.text == NodeStatus::Missing),
        "{lines:?}"
    );
}
