//! Compatibility with svn 1.15: the same history and the same operations, run once by the real
//! svn and once by this client (history replayed into the fake session from `svnadmin dump`),
//! must leave identical working copies: files, NODES rows, ACTUAL rows and `svn status` output.
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::oracle::*;
use common::*;
use opensvn_svn_oracle::Oracle;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth};

fn write(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).expect("mkdir");
    }
    std::fs::write(path, text).expect("write");
}

fn assert_same(oracle: &Oracle, theirs: &Path, ours: &Path, what: &str) {
    assert_eq!(tree_snapshot(ours), tree_snapshot(theirs), "{what}: working files differ");
    assert_eq!(nodes_snapshot(ours), nodes_snapshot(theirs), "{what}: NODES rows differ");
    assert_eq!(actual_snapshot(ours), actual_snapshot(theirs), "{what}: ACTUAL rows differ");
    assert_eq!(
        svn_status(oracle, ours, &[]),
        svn_status(oracle, theirs, &[]),
        "{what}: svn status differs"
    );
}

/// r1: files, properties, nested directories. Returns the seed working copy.
fn build_r1(repo: &Repo) -> std::path::PathBuf {
    let seed = repo.checkout("seed");
    write(&seed.join("README"), "hello\nworld\n");
    write(&seed.join("run.sh"), "#!/bin/sh\necho hi\n");
    write(&seed.join("docs/guide.txt"), "one\ntwo\nthree\nfour\nfive\nsix\nseven\n");
    write(&seed.join("src/lib/deep.rs"), "fn deep() {}\n");
    write(&seed.join("empty.txt"), "");
    repo.svn(&seed, &["add", "-q", "README", "run.sh", "docs", "src", "empty.txt"]);
    repo.svn(&seed, &["propset", "-q", "svn:executable", "*", "run.sh"]);
    repo.svn(&seed, &["propset", "-q", "svn:eol-style", "native", "docs/guide.txt"]);
    repo.svn(&seed, &["propset", "-q", "color", "red", "docs"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    seed
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_matches_svn_checkout() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(&ctx(), &repo.fake(None), &ours, None, Depth::Infinity, false, &CancelToken::new())
        .await
        .expect("checkout");
    assert_same(&oracle, &theirs, &ours, "checkout");
    // svn itself must accept the working copy we wrote.
    assert_eq!(repo.svn(&ours, &["status"]).trim(), "");
    repo.svn(&ours, &["cleanup"]);
    repo.svn(&ours, &["diff"]);
    assert_eq!(repo.svn(&ours, &["info", "--show-item", "revision"]).trim(), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_matches_svn_update() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(&ctx(), &repo.fake(Some(1)), &ours, None, Depth::Infinity, false, &CancelToken::new())
        .await
        .expect("checkout r1");

    // r2: edit, add, delete, property change, new directory.
    write(&seed.join("README"), "hello\nbrave\nworld\n");
    write(&seed.join("new/added.txt"), "added\n");
    repo.svn(&seed, &["add", "-q", "new"]);
    repo.svn(&seed, &["rm", "-q", "src"]);
    repo.svn(&seed, &["propset", "-q", "color", "blue", "docs"]);
    repo.svn(&seed, &["propset", "-q", "svn:eol-style", "LF", "README"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);

    repo.svn(&theirs, &["update", "-q"]);
    update(&ctx(), &repo.fake(None), &ours, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update");
    assert_same(&oracle, &theirs, &ours, "update");
    assert_eq!(repo.svn(&ours, &["status"]).trim(), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_with_local_changes_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(&ctx(), &repo.fake(Some(1)), &ours, None, Depth::Infinity, false, &CancelToken::new())
        .await
        .expect("checkout r1");

    // Identical local work in both copies.
    for wc in [&theirs, &ours] {
        write(&wc.join("docs/guide.txt"), "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\n");
        write(&wc.join("README"), "hello\nmine\n");
        write(&wc.join("scratch.txt"), "unversioned\n");
    }
    // r2 from the seed copy: a disjoint edit (clean merge) and an overlapping one (conflict).
    write(&seed.join("docs/guide.txt"), "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n");
    write(&seed.join("README"), "hello\ntheirs\n");
    write(&seed.join("added.txt"), "x\n");
    repo.svn(&seed, &["add", "-q", "added.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);

    repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
    update(&ctx(), &repo.fake(None), &ours, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update");
    assert_same(&oracle, &theirs, &ours, "update with local changes");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_with_property_edits_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(&ctx(), &repo.fake(Some(1)), &ours, None, Depth::Infinity, false, &CancelToken::new())
        .await
        .expect("checkout r1");
    for wc in [&theirs, &ours] {
        if wc == &theirs {
            repo.svn(wc, &["propset", "-q", "color", "green", "docs"]);
            repo.svn(wc, &["propset", "-q", "local", "1", "README"]);
        } else {
            let db = open_db(wc);
            db.set_props(&rp("docs"), &vec![(b"color".to_vec(), b"green".to_vec())], &[])
                .expect("propset");
            db.set_props(&rp("README"), &vec![(b"local".to_vec(), b"1".to_vec())], &[])
                .expect("propset");
        }
    }
    repo.svn(&seed, &["propset", "-q", "color", "blue", "docs"]);
    repo.svn(&seed, &["propset", "-q", "other", "x", "README"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
    update(&ctx(), &repo.fake(None), &ours, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update");
    assert_same(&oracle, &theirs, &ours, "property update");
}

/// Runs `local` on both copies, `server` on the seed (r2), then updates both and compares.
async fn scenario(
    name: &str,
    local: impl Fn(&Repo, &Path, bool),
    server: impl Fn(&Repo, &Path),
) -> Option<(Oracle, std::path::PathBuf, std::path::PathBuf)> {
    let oracle = oracle()?;
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    checkout(&ctx(), &repo.fake(Some(1)), &ours, None, Depth::Infinity, false, &CancelToken::new())
        .await
        .expect("checkout r1");
    local(&repo, &theirs, true);
    local(&repo, &ours, false);
    server(&repo, &seed);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
    update(&ctx(), &repo.fake(None), &ours, None, Depth::Unknown, &CancelToken::new())
        .await
        .unwrap_or_else(|e| panic!("{name}: update failed: {e}"));
    // Keep the sandbox and repository alive for the caller.
    std::mem::forget(sb);
    std::mem::forget(repo);
    Some((oracle, theirs, ours))
}

fn mime_binary(repo: &Repo, wc: &Path, is_svn: bool, file: &str) {
    if is_svn {
        repo.svn(wc, &["propset", "-q", "svn:mime-type", "application/octet-stream", file]);
    } else {
        open_db(wc)
            .set_props(
                &rp(file),
                &vec![(b"svn:mime-type".to_vec(), b"application/octet-stream".to_vec())],
                &[],
            )
            .expect("propset");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_conflict_matches_svn() {
    let Some((o, t, w)) = scenario(
        "binary",
        |repo, wc, is_svn| {
            mime_binary(repo, wc, is_svn, "empty.txt");
            write(&wc.join("empty.txt"), "mine\n");
        },
        |_, seed| write(&seed.join("empty.txt"), "theirs\n"),
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "binary conflict");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_of_unmodified_directory_with_unversioned_leftovers_matches_svn() {
    let Some((o, t, w)) = scenario(
        "rm-dir",
        |_, wc, _| write(&wc.join("src/lib/leftover.txt"), "keep me\n"),
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "src"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "directory delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_edited_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "edit-vs-delete",
        |_, wc, _| write(&wc.join("README"), "locally edited\n"),
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "README"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "edit vs delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_edit_of_locally_deleted_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "delete-vs-edit",
        |repo, wc, is_svn| {
            if is_svn {
                repo.svn(wc, &["rm", "-q", "README"]);
            } else {
                open_db(wc).op_delete(&rp("README"), &[]).expect("delete");
                std::fs::remove_file(wc.join("README")).expect("rm");
            }
        },
        |_, seed| write(&seed.join("README"), "server edit\n"),
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "delete vs edit");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_add_over_unversioned_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "obstruction",
        |_, wc, _| write(&wc.join("added.txt"), "local\n"),
        |repo, seed| {
            write(&seed.join("added.txt"), "server\n");
            repo.svn(seed, &["add", "-q", "added.txt"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "obstruction");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_file_is_restored_by_update_like_svn() {
    let Some((o, t, w)) = scenario(
        "missing",
        |_, wc, _| std::fs::remove_file(wc.join("README")).expect("rm"),
        |_, seed| write(&seed.join("README"), "server edit\n"),
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "missing file");
}
