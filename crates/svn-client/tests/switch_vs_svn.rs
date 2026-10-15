//! `svn switch` against svn 1.15: the same repository history (replayed into the fake session,
//! copies included) and the same working copy, then `svn switch` on one checkout and
//! `svn_client::switch` on the other. Files, NODES and ACTUAL rows and `svn status` must be
//! identical, and a switch svn refuses must be refused too, leaving both copies as they were.
//!
//! - Excluded children (`svn update --set-depth exclude`) stay excluded at the new location, at
//!   every switch depth, also when svn leaves them at the old location (outside the depth) or
//!   removes them (gone on the branch).
//! - The ancestry check: a location that shares no history with the working copy is refused
//!   (E195012) unless ancestry is ignored; branches and tags of the same line pass.
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::oracle::*;
use common::*;
use opensvn_svn_oracle::Oracle;
use svn_client::{checkout, set_depth};
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RepoUrl, Revnum};

fn url(rel: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url")
}

fn depth_word(d: Depth) -> Option<&'static str> {
    match d {
        Depth::Empty => Some("empty"),
        Depth::Files => Some("files"),
        Depth::Immediates => Some("immediates"),
        Depth::Infinity => Some("infinity"),
        _ => None,
    }
}

/// Runs the real svn and returns its exit status and stderr.
fn svn_result(oracle: &Oracle, cwd: &Path, args: &[&str]) -> (bool, String) {
    let mut c: Command = oracle.svn();
    c.args(args).current_dir(cwd);
    let out = c.output().expect("spawn svn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// r1: `trunk` with files, nested directories and a property. r2: the branch `branches/b`
/// (a copy of trunk@1) and trunk edits. r3: branch edits (a deleted directory, a new file, an
/// edit inside a directory the working copies exclude).
fn build(repo: &Repo) -> PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("trunk/keep.txt"), "keep\n");
    write(&seed.join("trunk/f.txt"), "f\n");
    write(&seed.join("trunk/X/x.txt"), "x\n");
    write(&seed.join("trunk/Y/y.txt"), "y\n");
    write(&seed.join("trunk/Y/Z/z.txt"), "z\n");
    write(&seed.join("trunk/gone/g.txt"), "g\n");
    std::fs::create_dir_all(seed.join("branches")).expect("mkdir");
    repo.svn(&seed, &["add", "-q", "trunk", "branches"]);
    repo.svn(&seed, &["propset", "-q", "color", "red", "trunk/Y"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["copy", "-q", "trunk", "branches/b"]);
    write(&seed.join("trunk/keep.txt"), "keep\ntrunk r2\n");
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&seed, &["rm", "-q", "branches/b/gone", "branches/b/Y/Z"]);
    write(&seed.join("branches/b/X/x.txt"), "x\nbranch r3\n");
    write(&seed.join("branches/b/X/new.txt"), "new\n");
    write(&seed.join("branches/b/f.txt"), "f\nbranch r3\n");
    repo.svn(&seed, &["add", "-q", "branches/b/X/new.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
    seed
}

/// Two checkouts of `trunk@1` (svn's and ours, through a session at trunk like the engine's).
struct Pair {
    oracle: Oracle,
    repo: Repo,
    fake: FakeRaSession,
    theirs: PathBuf,
    ours: PathBuf,
    _sb: Sandbox,
}

impl Pair {
    async fn new(build: impl Fn(&Repo) -> PathBuf, from: &str, rev: u64) -> Option<Self> {
        let oracle = oracle()?;
        let repo = Repo::new(&oracle);
        build(&repo);
        let theirs = repo.base().join("svnwc");
        let from_url = format!("{}/{from}@{rev}", repo.url);
        repo.svn(
            &repo.base(),
            &["checkout", "-q", &from_url, theirs.to_str().unwrap()],
        );
        let fake = repo.fake(None);
        let sb = Sandbox::new();
        let ours = sb.path("wc");
        checkout(
            &ctx(),
            &fake.at(&url(from)).unwrap(),
            &ours,
            Some(Revnum(rev)),
            Depth::Infinity,
            false,
            &CancelToken::new(),
        )
        .await
        .expect("checkout");
        Some(Self {
            oracle,
            repo,
            fake,
            theirs,
            ours,
            _sb: sb,
        })
    }

    /// Excludes `path` (working copy relative) in both copies.
    async fn exclude(&self, from: &str, path: &str) {
        self.repo.svn(
            &self.theirs,
            &["update", "-q", "--set-depth", "exclude", path],
        );
        let at = self.fake.at(&url(&format!("{from}/{path}"))).unwrap();
        set_depth(
            &ctx(),
            &at,
            &self.ours.join(path),
            None,
            Depth::Exclude,
            &CancelToken::new(),
        )
        .await
        .expect("exclude");
    }

    /// Switches the item `path` of both copies to `to@rev`; returns svn's outcome and ours.
    async fn switch(
        &self,
        from: &str,
        path: &str,
        to: &str,
        rev: u64,
        depth: Depth,
        ignore_ancestry: bool,
    ) -> ((bool, String), svn_types::Result<svn_client::UpdateSummary>) {
        // `URL@REV`: the location in that revision, as `svn_client::switch` reads `url` and
        // `revision` (`-r REV URL` would trace `URL@HEAD` back instead).
        let to_url = format!("{}/{to}@{rev}", self.repo.url);
        let mut args = vec!["switch", "-q", to_url.as_str()];
        let target = if path.is_empty() { "." } else { path };
        args.push(target);
        if ignore_ancestry {
            args.push("--ignore-ancestry");
        }
        let d = depth_word(depth).map(|w| format!("--depth={w}"));
        if let Some(d) = &d {
            args.push(d);
        }
        let theirs = svn_result(&self.oracle, &self.theirs, &args);
        // The engine's session sits at the working copy's URL.
        let at = self.fake.at(&url(from)).unwrap();
        let ours = svn_client::switch(
            &ctx(),
            &at,
            &self.ours.join(path),
            &url(to),
            Some(Revnum(rev)),
            depth,
            ignore_ancestry,
            &CancelToken::new(),
        )
        .await;
        (theirs, ours)
    }

    fn assert_same(&self, what: &str) {
        assert_same(&self.oracle, &self.theirs, &self.ours, what);
    }

    /// The BASE row of `path` in our copy: `(presence, revision, repository path)`.
    fn row(&self, path: &str) -> Option<(String, Option<u64>, Option<String>)> {
        let db = open_db(&self.ours);
        db.read_base(&rp(path)).expect("read").map(|n| {
            (
                format!("{:?}", n.presence),
                n.revision,
                n.repos_relpath.clone(),
            )
        })
    }
}

// ---- excluded children ---------------------------------------------------------------------

/// Excludes `excluded` below a trunk checkout, switches the whole working copy (as the engine
/// does) to the branch at r3 with `depth`, and compares with svn.
async fn excluded_children_after_switch(excluded: &[&str], depth: Depth) -> Option<Pair> {
    let p = Pair::new(build, "trunk", 1).await?;
    for e in excluded {
        p.exclude("trunk", e).await;
    }
    p.assert_same("before the switch");
    let (theirs, ours) = p.switch("trunk", "", "branches/b", 3, depth, false).await;
    assert!(theirs.0, "svn switch failed: {}", theirs.1);
    ours.expect("switch");
    p.assert_same(&format!(
        "switch at depth {depth:?} with {excluded:?} excluded"
    ));
    Some(p)
}

#[tokio::test(flavor = "multi_thread")]
async fn excluded_children_stay_excluded_at_the_new_location() {
    let Some(p) = excluded_children_after_switch(&["X", "f.txt", "Y/Z"], Depth::Unknown).await
    else {
        return;
    };
    // `X` changed on the branch, `f.txt` too: still excluded, now at the branch, at r3.
    // `Y/Z` is gone on the branch: svn drops its row, like any node the server deletes.
    assert_eq!(
        p.row("X"),
        Some(("Excluded".into(), Some(3), Some("branches/b/X".into())))
    );
    assert_eq!(
        p.row("f.txt"),
        Some(("Excluded".into(), Some(3), Some("branches/b/f.txt".into())))
    );
    assert_eq!(p.row("Y/Z"), None);
    assert!(!p.ours.join("X").exists() && !p.ours.join("f.txt").exists());
    // A later update keeps them excluded.
    let at = p.fake.at(&url("branches/b")).unwrap();
    p.repo.svn(&p.theirs, &["update", "-q"]);
    svn_client::update(
        &ctx(),
        &at,
        &p.ours,
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("update");
    p.assert_same("update after the switch");
    assert_eq!(p.row("X").map(|r| r.0), Some("Excluded".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn excluded_child_of_a_directory_gone_on_the_branch_goes_with_it() {
    excluded_children_after_switch(&["gone/g.txt"], Depth::Unknown).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn excluded_children_with_a_limited_switch_depth_match_svn() {
    // Outside the depth svn leaves nodes, excluded ones included, at their old location.
    for depth in [
        Depth::Empty,
        Depth::Files,
        Depth::Immediates,
        Depth::Infinity,
    ] {
        excluded_children_after_switch(&["X", "f.txt", "Y/Z"], depth).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn excluded_child_of_a_switched_subdirectory_matches_svn() {
    let Some(p) = Pair::new(build, "trunk", 1).await else {
        return;
    };
    p.exclude("trunk", "Y/Z").await;
    p.exclude("trunk", "X/x.txt").await;
    // A subdirectory switched on its own: the session still sits at the working copy root.
    let (theirs, ours) = p
        .switch("trunk", "X", "branches/b/X", 3, Depth::Unknown, false)
        .await;
    assert!(theirs.0, "svn switch failed: {}", theirs.1);
    ours.expect("switch X");
    p.assert_same("switch X");
    assert_eq!(
        p.row("X/x.txt"),
        Some((
            "Excluded".into(),
            Some(3),
            Some("branches/b/X/x.txt".into())
        ))
    );
    // `Y/Z` is outside the switched subtree: untouched.
    assert_eq!(
        p.row("Y/Z"),
        Some(("Excluded".into(), Some(1), Some("trunk/Y/Z".into())))
    );
}

// ---- ancestry ------------------------------------------------------------------------------

/// r1 `trunk`; r2 the branch `branches/b` (copy of trunk@1) and `other` (unrelated, added on
/// its own); r3 the tag `tags/t` (copy of the branch); r4 `branches/b` deleted; r5
/// `branches/b` added again, on its own (same name, unrelated history); r6 the file copy
/// `trunk/copy.txt` (from `trunk/f.txt`).
fn build_ancestry(repo: &Repo) -> PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("trunk/f.txt"), "f\n");
    write(&seed.join("trunk/sub/s.txt"), "s\n");
    std::fs::create_dir_all(seed.join("branches")).expect("mkdir");
    std::fs::create_dir_all(seed.join("tags")).expect("mkdir");
    repo.svn(&seed, &["add", "-q", "trunk", "branches", "tags"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["copy", "-q", "trunk", "branches/b"]);
    write(&seed.join("other/f.txt"), "other\n");
    write(&seed.join("other/sub/s.txt"), "other s\n");
    repo.svn(&seed, &["add", "-q", "other"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["copy", "-q", "branches/b", "tags/t"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
    repo.svn(&seed, &["rm", "-q", "branches/b"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r4"]);
    write(&seed.join("branches/b/f.txt"), "new b\n");
    repo.svn(&seed, &["add", "-q", "branches/b"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r5"]);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(&seed, &["copy", "-q", "trunk/f.txt", "trunk/copy.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r6"]);
    seed
}

/// Switches `path` of a checkout of `from@rev` to `to@to_rev`; svn and this client must agree
/// on refusing it, and leave identical working copies either way.
async fn ancestry_case(from: &str, rev: u64, path: &str, to: &str, to_rev: u64, related: bool) {
    let Some(p) = Pair::new(build_ancestry, from, rev).await else {
        return;
    };
    let what = format!("{from}@{rev} {path:?} -> {to}@{to_rev}");
    let (theirs, ours) = p
        .switch(from, path, to, to_rev, Depth::Unknown, false)
        .await;
    if related {
        assert!(theirs.0, "{what}: svn switch failed: {}", theirs.1);
        ours.unwrap_or_else(|e| panic!("{what}: switch: {e}"));
    } else {
        assert!(!theirs.0, "{what}: svn switched");
        assert!(theirs.1.contains("E195012"), "{what}: {}", theirs.1);
        let err = ours.err().unwrap_or_else(|| panic!("{what}: switched"));
        assert_eq!(err.code, ErrorCode::NoCommonAncestry, "{what}: {err}");
        assert_eq!(err.svn_code, Some(195_012), "{what}: {err}");
    }
    p.assert_same(&what);
    if !related {
        // With ancestry ignored both switch.
        let (theirs, ours) = p.switch(from, path, to, to_rev, Depth::Unknown, true).await;
        assert!(
            theirs.0,
            "{what}: svn --ignore-ancestry failed: {}",
            theirs.1
        );
        ours.unwrap_or_else(|e| panic!("{what}: switch ignoring ancestry: {e}"));
        p.assert_same(&format!("{what} --ignore-ancestry"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_to_a_branch_or_tag_of_the_same_line_is_allowed() {
    ancestry_case("trunk", 2, "", "branches/b", 2, true).await;
    ancestry_case("trunk", 3, "", "tags/t", 3, true).await;
    // From the tag back to the trunk it was made from (two copies away).
    ancestry_case("tags/t", 3, "", "trunk", 6, true).await;
    // A subdirectory, and a file copied from the switched file.
    ancestry_case("trunk", 2, "sub", "branches/b/sub", 2, true).await;
    ancestry_case("trunk", 6, "f.txt", "trunk/copy.txt", 6, true).await;
    // The same location at another revision.
    ancestry_case("trunk", 1, "", "trunk", 6, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_to_an_unrelated_location_is_refused_unless_ancestry_is_ignored() {
    ancestry_case("trunk", 2, "", "other", 2, false).await;
    ancestry_case("trunk", 2, "sub", "other/sub", 2, false).await;
    ancestry_case("trunk", 2, "f.txt", "other/f.txt", 2, false).await;
    // `branches/b` deleted and added again: the new one shares nothing with trunk, nor with
    // the old branch of the same name.
    ancestry_case("trunk", 5, "", "branches/b", 5, false).await;
    ancestry_case("branches/b", 3, "", "branches/b", 5, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fake_follows_copies_like_the_repository() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_ancestry(&repo);
    let fake = repo.fake(None);
    let cancel = CancelToken::new();
    let segs = |path: &str, peg: u64| {
        let fake = fake.clone();
        let cancel = cancel.clone();
        let path = rp(path);
        async move {
            svn_ra_api::RaSession::location_segments(&fake, &path, Revnum(peg), &cancel)
                .await
                .expect("segments")
                .into_iter()
                .map(|s| {
                    format!(
                        "{}:{}-{}",
                        s.path.map(|p| p.as_str().to_owned()).unwrap_or_default(),
                        s.range_start.0,
                        s.range_end.0
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    // Copies are followed back to their sources, youngest segment first.
    assert_eq!(
        segs("tags/t/sub/s.txt", 6).await,
        [
            "/tags/t/sub/s.txt:3-6",
            "/branches/b/sub/s.txt:2-2",
            "/trunk/sub/s.txt:1-1"
        ]
    );
    assert_eq!(segs("branches/b", 6).await, ["/branches/b:5-6"]);
    assert_eq!(
        segs("trunk/copy.txt", 6).await,
        ["/trunk/copy.txt:6-6", "/trunk/f.txt:1-5"]
    );
    assert_eq!(segs("", 6).await, ["/:0-6"]);
    let err =
        svn_ra_api::RaSession::location_segments(&fake, &rp("branches/b"), Revnum(4), &cancel)
            .await
            .unwrap_err();
    assert_eq!(err.code, ErrorCode::PathNotFound);
}
