//! `svn switch`'s ancestry check, against svn 1.15.
//!
//! svn refuses to switch to a location that shares no history with the working copy (E195012,
//! `svn_client__switch_internal`) unless `--ignore-ancestry` is given. The repository is made by
//! svn with a real copy (`svn cp`), so the fake replays the copy history and answers the
//! location-segment questions the check asks.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::checkout;
use svn_ra_api::RaSession as _;
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, ErrorCode, RepoUrl, Revnum};

/// r1: `trunk`; r2: `branches/b` copied from trunk (`svn cp`); r3: an edit on the branch;
/// r4: `other`, the same files added on their own (no history in common with trunk).
fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("trunk/README"), "hello\nworld\n");
    write(&seed.join("trunk/src/main.c"), "int main() { return 0; }\n");
    repo.svn(&seed, &["add", "-q", "trunk"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["mkdir", "-q", "branches"]);
    repo.svn(&seed, &["copy", "-q", "trunk", "branches/b"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2: branch"]);
    write(
        &seed.join("branches/b/src/main.c"),
        "int main() { return 1; }\n",
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r3: on the branch"]);
    write(&seed.join("other/README"), "hello\nworld\n");
    write(&seed.join("other/src/main.c"), "int main() { return 2; }\n");
    repo.svn(&seed, &["add", "-q", "other"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r4: unrelated"]);
}

/// svn's and our working copies of trunk at HEAD, each with the same local edit, and the fake
/// session anchored at trunk.
async fn trunk_checkouts(
    repo: &Repo,
    sb: &Sandbox,
) -> (std::path::PathBuf, std::path::PathBuf, FakeRaSession) {
    let theirs = repo.base().join("svnwc");
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            &format!("{}/trunk", repo.url),
            theirs.to_str().unwrap(),
        ],
    );
    let fake = repo.fake(None);
    let at_trunk = fake.at(&url("trunk")).expect("session at trunk");
    let ours = sb.path("wc");
    checkout(
        &ctx(),
        &at_trunk,
        &ours,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    for wc in [&theirs, &ours] {
        write(&wc.join("README"), "hello\nworld\nlocal edit\n");
    }
    (theirs, ours, at_trunk)
}

fn url(rel: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url")
}

/// Runs `svn <args>` in `cwd` and returns its stderr; the command must fail.
fn svn_fails(repo: &Repo, cwd: &Path, args: &[&str]) -> String {
    let out = repo
        .oracle
        .svn()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn");
    assert!(!out.status.success(), "svn {args:?} succeeded");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_to_a_branch_copied_from_trunk_passes_the_ancestry_check() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let (theirs, ours, at_trunk) = trunk_checkouts(&repo, &sb).await;

    repo.svn(
        &theirs,
        &["switch", "-q", &format!("{}/branches/b", repo.url)],
    );
    svn_client::switch(
        &ctx(),
        &at_trunk,
        &ours,
        &url("branches/b"),
        None,
        Depth::Unknown,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("switch to the branch");
    assert_same(&oracle, &theirs, &ours, "switch to a related branch");
    assert_eq!(read(&ours.join("src/main.c")), "int main() { return 1; }\n");
    assert_eq!(read(&ours.join("README")), "hello\nworld\nlocal edit\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_to_an_unrelated_location_is_refused_then_works_with_ignore_ancestry() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let (theirs, ours, at_trunk) = trunk_checkouts(&repo, &sb).await;
    let other = format!("{}/other", repo.url);

    let stderr = svn_fails(&repo, &theirs, &["switch", "-q", &other]);
    assert!(stderr.contains("E195012"), "svn: {stderr}");
    let err = svn_client::switch(
        &ctx(),
        &at_trunk,
        &ours,
        &url("other"),
        None,
        Depth::Unknown,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("an unrelated switch is refused");
    assert_eq!(err.code, ErrorCode::UnrelatedResources, "{err}");
    assert_eq!(err.svn_code, Some(195_012), "{err}");
    // Nothing changed on either side: same rows, same files, the local edit still there.
    assert_same(&oracle, &theirs, &ours, "refused unrelated switch");
    assert_eq!(read(&ours.join("README")), "hello\nworld\nlocal edit\n");
    assert_eq!(
        repo.svn(&ours, &["info", "--show-item", "relative-url"])
            .trim(),
        "^/trunk"
    );

    repo.svn(&theirs, &["switch", "-q", "--ignore-ancestry", &other]);
    svn_client::switch(
        &ctx(),
        &at_trunk,
        &ours,
        &url("other"),
        None,
        Depth::Unknown,
        true,
        &CancelToken::new(),
    )
    .await
    .expect("switch with ignore-ancestry");
    assert_same(&oracle, &theirs, &ours, "switch ignoring ancestry");
    assert_eq!(read(&ours.join("src/main.c")), "int main() { return 2; }\n");
    assert_eq!(read(&ours.join("README")), "hello\nworld\nlocal edit\n");
}

/// The fake replays svn's copy as history: the branch traces back to trunk, the unrelated tree
/// does not.
#[tokio::test(flavor = "multi_thread")]
async fn replayed_copies_keep_their_location_segments() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let fake = repo.fake(None);
    let cancel = CancelToken::new();
    let segs = |p: &str| {
        let fake = fake.clone();
        let cancel = cancel.clone();
        let p = rp(p);
        async move {
            fake.get_location_segments(&p, None, None, None, &cancel)
                .await
                .expect("segments")
                .into_iter()
                .map(|s| {
                    (
                        s.path.map(|p| p.as_str().to_owned()),
                        s.range_start.0,
                        s.range_end.0,
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        segs("branches/b/src/main.c").await,
        vec![
            (Some("branches/b/src/main.c".to_owned()), 2, 4),
            (Some("trunk/src/main.c".to_owned()), 1, 1),
        ]
    );
    assert_eq!(segs("other").await, vec![(Some("other".to_owned()), 4, 4)]);
    let related = svn_client::youngest_common_ancestor(
        &fake,
        &svn_client::Location {
            relpath: rp("branches/b"),
            rev: Revnum(4),
        },
        &svn_client::Location {
            relpath: rp("trunk"),
            rev: Revnum(4),
        },
        &cancel,
    )
    .await
    .expect("yca");
    assert_eq!(
        related,
        Some(svn_client::Location {
            relpath: rp("trunk"),
            rev: Revnum(1),
        })
    );
}
