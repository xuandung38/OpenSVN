//! An update whose target is an empty directory on disk while wc.db still has working rows below
//! it, compared with svn 1.15: working files, NODES and ACTUAL rows and `svn status`.
//!
//! svn treats such a target as a clean checkout (`is_empty_wc`): its editor adds the incoming
//! nodes without reading wc.db. The database layer still installs each file from its new
//! pristine and extends the `base-deleted` rows of a local working layer over the new BASE
//! nodes below it. A local add of the same name stays as a replacement of the new BASE node.
//!
//! r1: `A/E/x.txt`, `A/E/y.txt`. r2: the server adds `A/E/new.txt` and `A/E/sub/s.txt` and
//! replaces `A/E/x.txt` by a directory. At r1 the user deletes `x.txt` and `y.txt`, adds
//! `new.txt` (with a property and a changelist) and makes the directory `sub` (with an added
//! child), then removes `new.txt` and `sub` from disk.
//!
//! Skipped when no svn 1.14+ is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`); the
//! case with working rows below the target needs svn 1.15 (1.14 fails it itself).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::fs;
use std::path::Path;

use common::oracle::*;
use common::*;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, Revnum};

fn build(repo: &Repo) {
    let seed = repo.checkout("seed");
    write(&seed.join("A/E/x.txt"), "x\n");
    write(&seed.join("A/E/y.txt"), "y\n");
    write(&seed.join("A/keep.txt"), "keep\n");
    repo.svn(&seed, &["add", "-q", "A"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    repo.svn(&seed, &["rm", "-q", "A/E/x.txt"]);
    write(&seed.join("A/E/x.txt/in.txt"), "in\n");
    write(&seed.join("A/E/new.txt"), "server new\n");
    write(&seed.join("A/E/sub/s.txt"), "s\n");
    repo.svn(&seed, &["add", "-q", "A/E/x.txt", "A/E/new.txt", "A/E/sub"]);
    repo.svn(
        &seed,
        &["propset", "-q", "sp", "server prop", "A/E/new.txt"],
    );
    repo.svn(
        &seed,
        &["propset", "-q", "svn:executable", "*", "A/E/new.txt"],
    );
    repo.svn(
        &seed,
        &["propset", "-q", "svn:eol-style", "native", "A/E/new.txt"],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// Local states of `A/E` before the update; each leaves `A/E` an empty directory on disk.
#[derive(Debug, Clone, Copy)]
enum Local {
    /// Local deletes, an added file and a made directory, both gone from disk.
    WithDeletes,
    /// As `WithDeletes`, the added file with translation properties of its own.
    TranslatedAdd,
    /// The added file and directory, nothing deleted: `x.txt` and `y.txt` are missing instead.
    OnlyAdds,
}

const ALL: [Local; 3] = [Local::WithDeletes, Local::TranslatedAdd, Local::OnlyAdds];

fn prepare(repo: &Repo, wc: &Path, local: Local) {
    let e = wc.join("A/E");
    if !matches!(local, Local::OnlyAdds) {
        repo.svn(&e, &["rm", "-q", "x.txt", "y.txt"]);
    } else {
        // Nothing deleted: the versioned files leave the disk (missing), so `E` is empty.
        fs::remove_file(e.join("x.txt")).expect("rm x");
        fs::remove_file(e.join("y.txt")).expect("rm y");
    }
    write(&e.join("new.txt"), "my new\n");
    repo.svn(&e, &["add", "-q", "new.txt"]);
    repo.svn(&e, &["propset", "-q", "lp", "local prop", "new.txt"]);
    repo.svn(&e, &["changelist", "-q", "cl", "new.txt"]);
    if let Local::TranslatedAdd = local {
        repo.svn(&e, &["propset", "-q", "svn:eol-style", "CRLF", "new.txt"]);
        repo.svn(&e, &["propset", "-q", "svn:keywords", "Rev", "new.txt"]);
    }
    repo.svn(&e, &["mkdir", "-q", "sub"]);
    write(&e.join("sub/t.txt"), "t\n");
    repo.svn(&e, &["add", "-q", "sub/t.txt"]);
    fs::remove_file(e.join("new.txt")).expect("rm new.txt");
    fs::remove_dir_all(e.join("sub")).expect("rm sub");
    assert_eq!(
        fs::read_dir(&e).expect("read E").count(),
        0,
        "{local:?}: E is not empty"
    );
}

fn differences(oracle: &opensvn_svn_oracle::Oracle, theirs: &Path, ours: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |what: &str, a: String, b: String| {
        if a != b {
            out.push(format!("{what} differ:\n  ours:   {a}\n  theirs: {b}"));
        }
    };
    check(
        "working files",
        format!("{:?}", tree_snapshot(ours)),
        format!("{:?}", tree_snapshot(theirs)),
    );
    check(
        "NODES rows",
        nodes_snapshot(ours).join("\n          "),
        nodes_snapshot(theirs).join("\n          "),
    );
    check(
        "ACTUAL rows",
        actual_snapshot(ours).join("\n          "),
        actual_snapshot(theirs).join("\n          "),
    );
    check(
        "svn status",
        svn_status(oracle, ours, &[]).replace('\n', "\n          "),
        svn_status(oracle, theirs, &[]).replace('\n', "\n          "),
    );
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn update_of_an_empty_target_with_working_rows_below_matches_svn() {
    // svn 1.14.3 fails this update itself ("Can't install 'A/E/new.txt' from pristine store,
    // because no checksum is recorded"); 1.15 fixed it.
    let Some(oracle) = oracle_1_15() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let mut failures = Vec::new();
    for local in ALL {
        let name = format!("{local:?}").to_lowercase();
        let theirs = repo.base().join(format!("svn-{name}"));
        let target = theirs.to_str().expect("utf-8 path");
        repo.svn(
            &repo.base(),
            &["checkout", "-q", "-r", "1", &repo.url, target],
        );
        let ours = sb.path(&name);
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
        .expect("checkout r1");
        prepare(&repo, &theirs, local);
        prepare(&repo, &ours, local);

        repo.svn(&theirs, &["update", "-q", "A/E"]);
        let fake = repo.fake(None);
        update(
            &ctx(),
            &fake,
            &ours.join("A/E"),
            Some(Revnum(2)),
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{local:?}: update: {e}"));
        for d in differences(&oracle, &theirs, &ours) {
            failures.push(format!("{local:?}: {d}"));
        }
        // A second update has nothing left to do, in both clients.
        repo.svn(&theirs, &["update", "-q", "A/E"]);
        update(
            &ctx(),
            &fake,
            &ours.join("A/E"),
            Some(Revnum(2)),
            Depth::Unknown,
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{local:?}: second update: {e}"));
        for d in differences(&oracle, &theirs, &ours) {
            failures.push(format!("{local:?} (second update): {d}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
