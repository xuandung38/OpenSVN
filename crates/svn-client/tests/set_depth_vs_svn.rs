//! `svn update --set-depth` against svn 1.15: the same repository history (replayed into the
//! fake session) and the same local changes, then `svn update --set-depth` on one checkout and
//! `svn_client::set_depth` on the other. Files, NODES and ACTUAL rows and `svn status` must be
//! identical. Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::oracle::*;
use common::*;
use opensvn_svn_oracle::Oracle;
use svn_client::{checkout, set_depth, set_depth_parents};
use svn_types::{CancelToken, Depth, ErrorCode};

fn word(d: Depth) -> &'static str {
    match d {
        Depth::Empty => "empty",
        Depth::Files => "files",
        Depth::Immediates => "immediates",
        Depth::Exclude => "exclude",
        _ => "infinity",
    }
}

/// r1: a tree under `A` with something to crop at every level; r2 changes inside and outside it.
fn build(repo: &Repo) -> PathBuf {
    let seed = repo.checkout("seed");
    for d in ["X1", "X2", "X3", "X4", "X5", "X7", "K", "M", "N", "R"] {
        write(&seed.join(format!("A/{d}/f")), &format!("{d}\n"));
    }
    for f in ["f", "g", "h", "del", "kl", "miss"] {
        write(&seed.join(format!("A/{f}")), &format!("{f}\n"));
    }
    write(&seed.join("A/Y/y"), "y\n");
    write(&seed.join("A/Y/Z/z"), "z\n");
    write(&seed.join("top.txt"), "top\n");
    repo.svn(&seed, &["add", "-q", "A", "top.txt"]);
    repo.svn(&seed, &["propset", "-q", "svn:ignore", "*.o", "A/X3"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    write(&seed.join("A/new.txt"), "new\n");
    write(&seed.join("A/Y/new.txt"), "new y\n");
    write(&seed.join("A/X1/f"), "X1\nr2\n");
    write(&seed.join("top.txt"), "top\nr2\n");
    repo.svn(&seed, &["add", "-q", "A/new.txt", "A/Y/new.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    seed
}

/// Local changes of every kind crop has to respect, made with the real svn in both copies.
fn local_changes(repo: &Repo, wc: &Path) {
    let svn = |args: &[&str]| {
        repo.svn(wc, args);
    };
    write(&wc.join("A/f"), "f\nlocal\n");
    svn(&["propset", "-q", "p", "v", "A/h"]);
    write(&wc.join("A/X2/unv"), "unversioned\n");
    write(&wc.join("A/X3/ign.o"), "ignored\n");
    svn(&["propset", "-q", "p", "v", "A/X4"]);
    svn(&["rm", "-q", "A/X5/f"]);
    svn(&["changelist", "-q", "cl", "A/X7/f"]);
    write(&wc.join("A/Y/Z/z"), "z\nlocal\n");
    svn(&["rm", "-q", "--keep-local", "A/K/f"]);
    fs::remove_dir_all(wc.join("A/M")).expect("rm A/M");
    svn(&["mkdir", "-q", "A/N/added"]);
    svn(&["rm", "-q", "A/R"]);
    svn(&["mkdir", "-q", "A/R"]);
    svn(&["rm", "-q", "A/del"]);
    svn(&["rm", "-q", "--keep-local", "A/kl"]);
    fs::remove_file(wc.join("A/miss")).expect("rm A/miss");
}

struct Pair {
    oracle: Oracle,
    repo: Repo,
    theirs: PathBuf,
    ours: PathBuf,
    _sb: Sandbox,
}

/// Both copies at r1 (`depth`), with `local` applied to each.
async fn pair(depth: Depth, local: impl Fn(&Repo, &Path)) -> Option<Pair> {
    let oracle = oracle()?;
    let repo = Repo::new(&oracle);
    build(&repo);
    let theirs = repo.base().join("svnwc");
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            "-r",
            "1",
            "--depth",
            word(depth),
            &repo.url,
            theirs.to_str().unwrap(),
        ],
    );
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(
        &ctx(),
        &repo.fake(Some(1)),
        &ours,
        None,
        depth,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout r1");
    local(&repo, &theirs);
    local(&repo, &ours);
    assert_same(&oracle, &theirs, &ours, "before");
    Some(Pair {
        oracle,
        repo,
        theirs,
        ours,
        _sb: sb,
    })
}

impl Pair {
    async fn set_depth(&self, target: &str, depth: Depth) {
        let what = format!("--set-depth {} {target}", word(depth));
        let mut args = vec![
            "update",
            "-q",
            "--accept",
            "postpone",
            "--set-depth",
            word(depth),
        ];
        if !target.is_empty() {
            args.push(target);
        }
        self.repo.svn(&self.theirs, &args);
        set_depth(
            &ctx(),
            &self.repo.fake(None),
            &self.ours.join(target),
            None,
            depth,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        if depth == Depth::Exclude {
            // svn 1.15 leaves the removals queued (and the working copy locked) when it crops
            // instead of excluding; `svn cleanup` finishes them. svn_client::set_depth runs them.
            self.repo.svn(&self.theirs, &["cleanup"]);
        }
        assert_same(&self.oracle, &self.theirs, &self.ours, &what);
        // svn keeps working with the result.
        self.repo.svn(&self.ours, &["status"]);
        self.repo.svn(&self.ours, &["cleanup"]);
    }
}

impl Pair {
    /// `svn update --parents --set-depth` on `target` against `set_depth_parents`.
    async fn set_depth_parents(&self, target: &str, depth: Depth) {
        let what = format!("--parents --set-depth {} {target}", word(depth));
        self.repo.svn(
            &self.theirs,
            &[
                "update",
                "-q",
                "--accept",
                "postpone",
                "--parents",
                "--set-depth",
                word(depth),
                target,
            ],
        );
        set_depth_parents(
            &ctx(),
            &self.repo.fake(None),
            &self.ours.join(target),
            None,
            depth,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_same(&self.oracle, &self.theirs, &self.ours, &what);
        self.repo.svn(&self.ours, &["status"]);
        self.repo.svn(&self.ours, &["cleanup"]);
    }
}

async fn crop_then_grow(depth: Depth) {
    let Some(p) = pair(Depth::Infinity, local_changes).await else {
        return;
    };
    p.set_depth("A", depth).await;
    // Modified files are never removed.
    assert_eq!(read(&p.ours.join("A/f")), "f\nlocal\n");
    assert_eq!(read(&p.ours.join("A/Y/Z/z")), "z\nlocal\n");
    assert!(p.ours.join("A/X2/unv").exists());
    assert!(p.ours.join("A/X3/ign.o").exists());
    assert!(p.ours.join("A/kl").exists());
    p.set_depth("A", Depth::Infinity).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crop_to_empty_then_back_to_infinity() {
    crop_then_grow(Depth::Empty).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crop_to_files_then_back_to_infinity() {
    crop_then_grow(Depth::Files).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crop_to_immediates_then_back_to_infinity() {
    crop_then_grow(Depth::Immediates).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grow_a_sparse_checkout_step_by_step() {
    let Some(p) = pair(Depth::Empty, |_, _| {}).await else {
        return;
    };
    p.set_depth("", Depth::Files).await;
    p.set_depth("", Depth::Immediates).await;
    p.set_depth("A", Depth::Immediates).await;
    p.set_depth("A/Y", Depth::Infinity).await;
    p.set_depth("", Depth::Infinity).await;
}

/// Folders the working copy does not have (outside their parent's depth) come in at the
/// requested depth, as `svn update --set-depth` on a missing child does.
#[tokio::test(flavor = "multi_thread")]
async fn bring_in_folders_outside_the_parent_depth() {
    let Some(p) = pair(Depth::Empty, |_, _| {}).await else {
        return;
    };
    p.set_depth("A", Depth::Files).await;
    assert!(p.ours.join("A/f").exists());
    assert!(!p.ours.join("A/Y").exists());
    p.set_depth("A/Y", Depth::Empty).await;
    p.set_depth("A/X1", Depth::Infinity).await;
    p.set_depth("A/Y/Z", Depth::Immediates).await;
    // A file outside a `files` parent's depth cannot be missing; one outside `empty` can.
    p.set_depth("A/Y", Depth::Files).await;
    // Excluding a node the working copy does not have changes nothing.
    let before = open_db(&p.ours).read_base(&rp("A/X2")).unwrap();
    set_depth(
        &ctx(),
        &p.repo.fake(None),
        &p.ours.join("A/X2"),
        None,
        Depth::Exclude,
        &CancelToken::new(),
    )
    .await
    .expect("exclude a missing node");
    assert_eq!(open_db(&p.ours).read_base(&rp("A/X2")).unwrap(), before);
}

/// `--parents`: every missing folder above the target comes in at depth `empty`.
#[tokio::test(flavor = "multi_thread")]
async fn bring_in_nested_folders_with_parents() {
    let Some(p) = pair(Depth::Empty, |_, _| {}).await else {
        return;
    };
    p.set_depth_parents("A/Y/Z", Depth::Infinity).await;
    assert!(p.ours.join("A/Y/Z/z").exists());
    assert!(!p.ours.join("A/f").exists());
    p.set_depth_parents("A/X1", Depth::Files).await;
    // A target whose parent is present takes the plain path.
    p.set_depth_parents("A/X3", Depth::Empty).await;
}

/// What happens to a folder brought in below a shallower parent when the parent deepens.
#[tokio::test(flavor = "multi_thread")]
async fn deepen_parent_of_brought_in_folders() {
    let Some(p) = pair(Depth::Empty, |_, _| {}).await else {
        return;
    };
    p.set_depth_parents("A/Y", Depth::Infinity).await;
    p.set_depth("A", Depth::Immediates).await;
    // Like svn, deepening `empty` to `immediates` crops a folder brought in below it to `empty`.
    let y = open_db(&p.ours).read_base(&rp("A/Y")).unwrap().unwrap();
    assert_eq!(y.depth, Some(Depth::Empty));
    p.set_depth("A/Y", Depth::Exclude).await;
    p.set_depth_parents("A/X1/f", Depth::Empty).await;
    // Like svn, deepening `empty` to `files` drops a folder brought in below it.
    p.set_depth("A", Depth::Files).await;
    assert!(!p.ours.join("A/X1").exists());
    p.set_depth("A", Depth::Infinity).await;
}

/// Without `--parents` a target below a missing folder is refused and nothing changes.
#[tokio::test(flavor = "multi_thread")]
async fn missing_parent_is_refused() {
    let Some(p) = pair(Depth::Empty, |_, _| {}).await else {
        return;
    };
    let err = set_depth(
        &ctx(),
        &p.repo.fake(None),
        &p.ours.join("A/Y"),
        None,
        Depth::Infinity,
        &CancelToken::new(),
    )
    .await
    .expect_err("A is not in the working copy");
    assert_eq!(err.code, ErrorCode::PathNotFound, "{err}");
    assert_same(&p.oracle, &p.theirs, &p.ours, "after the refusal");
    let err = set_depth(
        &ctx(),
        &p.repo.fake(None),
        &p.ours.join("nope"),
        None,
        Depth::Infinity,
        &CancelToken::new(),
    )
    .await
    .expect_err("not in the repository");
    assert_eq!(err.code, ErrorCode::PathNotFound, "{err}");
}

/// An excluded folder comes back through its own `--set-depth`, not only its parent's.
#[tokio::test(flavor = "multi_thread")]
async fn excluded_folder_comes_back_on_its_own() {
    let Some(p) = pair(Depth::Infinity, |_, _| {}).await else {
        return;
    };
    p.set_depth("A/X1", Depth::Exclude).await;
    p.set_depth("A/Y", Depth::Exclude).await;
    p.set_depth("A/X1", Depth::Infinity).await;
    p.set_depth("A/Y", Depth::Immediates).await;
    assert!(p.ours.join("A/X1/f").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn exclude_and_bring_back() {
    let Some(p) = pair(Depth::Infinity, local_changes).await else {
        return;
    };
    // Unmodified: excluded. Modified below: cropped to empty instead.
    p.set_depth("A/X1", Depth::Exclude).await;
    p.set_depth("A/Y", Depth::Exclude).await;
    p.set_depth("A/g", Depth::Exclude).await;
    assert!(!p.ours.join("A/X1").exists());
    assert_eq!(read(&p.ours.join("A/Y/Z/z")), "z\nlocal\n");
    // The parent's depth brings the excluded nodes back.
    p.set_depth("A", Depth::Infinity).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals() {
    let Some(p) = pair(Depth::Infinity, local_changes).await else {
        return;
    };
    let fake = p.repo.fake(None);
    for (target, depth) in [
        ("A/f", Depth::Empty),       // a file has no depth
        ("A/N/added", Depth::Empty), // scheduled for addition
        ("A/R", Depth::Empty),       // replaced
        ("", Depth::Exclude),        // the root cannot be excluded
        ("A/kl", Depth::Exclude),    // scheduled for deletion
    ] {
        let err = set_depth(
            &ctx(),
            &fake,
            &p.ours.join(target),
            None,
            depth,
            &CancelToken::new(),
        )
        .await
        .expect_err(target);
        assert_eq!(err.code, ErrorCode::WcInvalidOperation, "{target}: {err}");
    }
}

/// The status columns svn prints for each of `paths` (relative to `wc`); `None` when it prints
/// nothing for the path.
fn svn_status_of(repo: &Repo, wc: &Path, paths: &[&str]) -> Vec<Option<String>> {
    let out = repo.svn(wc, &["status", "--ignore-externals"]);
    paths
        .iter()
        .map(|p| {
            out.lines()
                .find(|l| l.get(8..).is_some_and(|rest| rest == *p))
                .map(|l| l[..7].to_owned())
        })
        .collect()
}

/// Column `i` of a status line (`X` in column 0 for a directory external, in column 4 for a
/// file external).
fn col(status: &Option<String>, i: usize) -> Option<char> {
    status.as_deref().and_then(|s| s.chars().nth(i))
}

/// Review probe for #116: `A` holds a nested working copy with local edits and an unversioned
/// file, a directory external with a local edit, an unmodified directory external, a modified
/// file external, an unmodified one in a subfolder and an unversioned deep tree. `svn update
/// --set-depth` on `A` (exclude falls back to a crop because of the local changes) keeps every
/// local change and the nested and external `wc.db`s, and removes the externals whose
/// definition is no longer processed, like svn: what is left shows as `?`, not `X`.
#[tokio::test(flavor = "multi_thread")]
async fn crop_keeps_nested_working_copies_and_removes_externals_like_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build(&repo);
    repo.svn(&seed, &["update", "-q"]);
    repo.svn(
        &seed,
        &[
            "propset",
            "-q",
            "svn:externals",
            "^/A/Y XD\n^/A/Y XU\n^/top.txt fe\n",
            "A",
        ],
    );
    repo.svn(
        &seed,
        &["propset", "-q", "svn:externals", "^/top.txt fe3\n", "A/X2"],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r3"]);
    let fake = repo.fake(None);
    let opener = FakeOpener::new(vec![fake.clone()]);
    let cancel = CancelToken::new();

    for depth in [Depth::Empty, Depth::Files, Depth::Exclude] {
        let name = word(depth);
        let theirs = repo.checkout(&format!("svn-{name}"));
        let sb = Sandbox::new();
        let ours = sb.path("wc");
        checkout(
            &ext_ctx(opener.clone()),
            &fake,
            &ours,
            None,
            Depth::Infinity,
            false,
            &cancel,
        )
        .await
        .expect("checkout");
        let nested_url =
            svn_types::RepoUrl::parse(&format!("{}/A/Y", common::ROOT_URL)).expect("url");
        checkout(
            &ctx(),
            &fake.at(&nested_url).expect("at A/Y"),
            &ours.join("A/nested"),
            None,
            Depth::Infinity,
            false,
            &cancel,
        )
        .await
        .expect("nested checkout");
        repo.svn(
            &theirs,
            &["checkout", "-q", &format!("{}/A/Y", repo.url), "A/nested"],
        );
        for wc in [&theirs, &ours] {
            assert!(wc.join("A/XD/.svn/wc.db").exists(), "externals fetched");
            write(&wc.join("A/XD/y"), "y\nlocal\n");
            write(&wc.join("A/fe"), "top\nr2\nlocal\n");
            write(&wc.join("A/nested/y"), "y\nnested\n");
            write(&wc.join("A/nested/unv"), "unversioned\n");
            write(&wc.join("A/U/v/w"), "deep\n");
        }
        let paths = ["A/XD", "A/XU", "A/fe", "A/X2/fe3"];
        let before = svn_status_of(&repo, &ours, &paths);
        assert_eq!(before, svn_status_of(&repo, &theirs, &paths), "{name}");
        assert_eq!(col(&before[0], 0), Some('X'), "{before:?}");
        assert_eq!(col(&before[2], 4), Some('X'), "{before:?}");

        repo.svn(&theirs, &["update", "-q", "--set-depth", name, "A"]);
        repo.svn(&theirs, &["cleanup"]);
        let sum = set_depth(
            &ext_ctx(opener.clone()),
            &fake,
            &ours.join("A"),
            None,
            depth,
            &cancel,
        )
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(sum.externals.len(), 4, "{name}: {:?}", sum.externals);

        // Every local change is still there.
        assert_eq!(read(&ours.join("A/XD/y")), "y\nlocal\n", "{name}");
        assert!(ours.join("A/XD/.svn/wc.db").exists(), "{name}");
        assert_eq!(read(&ours.join("A/fe")), "top\nr2\nlocal\n", "{name}");
        assert_eq!(read(&ours.join("A/nested/y")), "y\nnested\n", "{name}");
        assert_eq!(read(&ours.join("A/nested/unv")), "unversioned\n", "{name}");
        assert!(ours.join("A/nested/.svn/wc.db").exists(), "{name}");
        assert_eq!(read(&ours.join("A/U/v/w")), "deep\n", "{name}");
        assert_eq!(
            repo.svn(&ours.join("A/nested"), &["status"]),
            repo.svn(&theirs.join("A/nested"), &["status"]),
            "{name}: the nested working copy is untouched"
        );

        // As in svn: the modified directory external stays as an unregistered working copy,
        // the unmodified one and the unmodified file external are gone.
        let ours_st = svn_status_of(&repo, &ours, &paths);
        let theirs_st = svn_status_of(&repo, &theirs, &paths);
        assert_eq!(col(&ours_st[0], 0), Some('?'), "{name}: {ours_st:?}");
        assert_eq!(ours_st[0], theirs_st[0], "{name}");
        assert_eq!(ours_st[1], theirs_st[1], "{name}: {ours_st:?}");
        assert!(!ours.join("A/XU").exists(), "{name}");
        assert_eq!(ours_st[3], theirs_st[3], "{name}: {ours_st:?}");
        assert!(!ours.join("A/X2/fe3").exists(), "{name}");
        // svn deletes the modified file external; this client keeps it, unversioned.
        assert_eq!(col(&ours_st[2], 0), Some('?'), "{name}: {ours_st:?}");
        let db = open_db(&ours);
        assert!(
            db.externals_under(&rp("")).expect("externals").is_empty(),
            "{name}"
        );
        // svn keeps working with the result.
        repo.svn(&ours, &["cleanup"]);
    }
}
