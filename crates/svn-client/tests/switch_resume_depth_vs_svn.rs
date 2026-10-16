//! A switch with a narrow `--depth` whose connection fails mid-drive, resumed at the switched
//! target, leaves the working copy svn 1.15's own switch at that depth leaves: nodes within the
//! depth at the branch, the ones below it (outside the depth) where they were, and a folder the
//! branch has outside the depth a `not-present` row. Files, NODES and
//! ACTUAL rows and `svn status` must be identical, whatever file the connection broke at.
//!
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

use common::oracle::*;
use common::*;
use rec_ra::RecRa;
use svn_client::{checkout, switch};
use svn_types::{CancelToken, ConnectionFailure, Depth, ErrorCode, RepoUrl, Revnum, SvnError};

fn broken() -> SvnError {
    SvnError::new(
        ErrorCode::Network,
        "HTTP request failed: error decoding response body: peer closed connection without \
         sending TLS close_notify",
    )
    .with_connection(ConnectionFailure::TlsClosedUncleanly)
}

fn url(rel: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url")
}

/// r1: `trunk` with files at the top, folders `d0..d2` with files and a subfolder `s` each.
/// r2: the branch. r3: every file of the branch edited, so each node the switch reaches changes.
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    let files = |root: &str, tag: &str| {
        let mut out = vec![(format!("{root}/top.txt"), format!("top {tag}\n"))];
        for d in 0..3 {
            for f in 0..3 {
                out.push((format!("{root}/d{d}/f{f}.txt"), format!("{d}.{f} {tag}\n")));
            }
            out.push((format!("{root}/d{d}/s/g.txt"), format!("{d}.g {tag}\n")));
        }
        out
    };
    for (p, t) in files("trunk", "trunk") {
        write(&seed.join(p), &t);
    }
    std::fs::create_dir_all(seed.join("branches")).expect("mkdir");
    repo.svn(&seed, &["add", "-q", "trunk", "branches"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["copy", "-q", "trunk", "branches/b"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    for (p, t) in files("branches/b", "branch") {
        write(&seed.join(p), &t);
    }
    // New on the branch: a folder at the top (outside `files`) and one inside `d0` (outside
    // `immediates`), a file at the top.
    write(&seed.join("branches/b/new/n.txt"), "n\n");
    write(&seed.join("branches/b/d0/inner/i.txt"), "i\n");
    write(&seed.join("branches/b/added.txt"), "a\n");
    repo.svn(
        &seed,
        &[
            "add",
            "-q",
            "branches/b/new",
            "branches/b/d0/inner",
            "branches/b/added.txt",
        ],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
}

fn word(d: Depth) -> &'static str {
    match d {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        _ => "infinity",
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_switch_with_a_narrow_depth_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let fake = repo.fake(None);
    let sb = Sandbox::new();
    for depth in [
        Depth::Empty,
        Depth::Files,
        Depth::Immediates,
        Depth::Infinity,
    ] {
        let theirs = repo.base().join(format!("svn-{}", word(depth)));
        let t = theirs.to_str().unwrap();
        repo.svn(
            &repo.base(),
            &["checkout", "-q", &format!("{}/trunk@3", repo.url), t],
        );
        repo.svn(
            &theirs,
            &[
                "switch",
                "-q",
                "--depth",
                word(depth),
                &format!("{}/branches/b@3", repo.url),
                ".",
            ],
        );
        // Break after every number of files the drive installs, up to past the end.
        let mut resumed_at_target = 0;
        for after in std::iter::once(usize::MAX).chain(0..12) {
            let what = format!("--depth {} broken after {after} files", word(depth));
            let ours = sb.path(&format!("ours-{}-{after}", word(depth)));
            checkout(
                &ctx(),
                &fake.at(&url("trunk")).unwrap(),
                &ours,
                Some(Revnum(3)),
                Depth::Infinity,
                false,
                &CancelToken::new(),
            )
            .await
            .expect("checkout");
            let ra = RecRa::new(fake.at(&url("trunk")).unwrap());
            ra.update_breaks
                .lock()
                .unwrap()
                .extend([Some((after, broken())), None, None]);
            switch(
                &ctx(),
                &ra,
                &ours,
                &url("branches/b"),
                Some(Revnum(3)),
                depth,
                true,
                &CancelToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{what}: {e:?}"));
            assert_same(&oracle, &theirs, &ours, &what);
            let log = ra.log.lock().unwrap().clone();
            if log.iter().any(|l| l.starts_with("reparented ")) {
                resumed_at_target += 1;
            }
            eprintln!("{what}: {:?}", ra.reports.lock().unwrap());
        }
        assert!(
            resumed_at_target > 0,
            "--depth {}: never resumed at the target",
            word(depth)
        );
    }
}
