//! `svn switch` of a directory with an excluded child against svn 1.15: the exclusion moves to
//! the new location (`Depth: exclude` under the switched URL) instead of being dropped. Files,
//! NODES and ACTUAL rows and `svn status` must be identical. Skipped when no svn 1.15 is
//! installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use common::oracle::*;
use common::*;
use svn_client::{checkout, set_depth, switch};
use svn_types::{CancelToken, Depth, RepoUrl, Revnum};

#[tokio::test(flavor = "multi_thread")]
async fn switch_keeps_an_excluded_child() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = repo.checkout("seed");
    write(&seed.join("A/f"), "f\n");
    write(&seed.join("A/X/x"), "x\n");
    write(&seed.join("A/Y/y"), "y\n");
    repo.svn(&seed, &["add", "-q", "A"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    // B: the same tree, added on its own (the fake session replays no copies; svn switches with
    // --ignore-ancestry, as the engine's drive does here).
    write(&seed.join("B/f"), "f\n");
    write(&seed.join("B/X/x"), "x\n");
    write(&seed.join("B/Y/y"), "y\n");
    repo.svn(&seed, &["add", "-q", "B"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    write(&seed.join("B/X/x"), "x\nr3\n");
    write(&seed.join("B/Y/y"), "y\nr3\n");
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);

    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    let fake = repo.fake(None);
    let cancel = CancelToken::new();
    checkout(&ctx(), &fake, &ours, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    repo.svn(&theirs, &["update", "-q", "--set-depth", "exclude", "A/X"]);
    set_depth(
        &ctx(),
        &fake,
        &ours.join("A/X"),
        None,
        Depth::Exclude,
        &cancel,
    )
    .await
    .expect("exclude");
    assert_same(&oracle, &theirs, &ours, "exclude");

    let b = RepoUrl::parse(&format!("{}/B", common::ROOT_URL)).expect("url");
    // To r2 (B as added) and then to r3 (X changed on B): X stays excluded both times.
    for rev in [2, 3] {
        repo.svn(
            &theirs,
            &[
                "switch",
                "-q",
                "--ignore-ancestry",
                "-r",
                &rev.to_string(),
                &format!("{}/B", repo.url),
                "A",
            ],
        );
        switch(
            &ctx(),
            &fake,
            &ours.join("A"),
            &b,
            Some(Revnum(rev)),
            Depth::Unknown,
            &cancel,
        )
        .await
        .expect("switch");
        assert_same(&oracle, &theirs, &ours, &format!("switch to B@{rev}"));
    }
    assert!(!ours.join("A/X").exists());
    assert_eq!(read(&ours.join("A/Y/y")), "y\nr3\n");
}
