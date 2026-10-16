//! Compatibility with svn 1.15: the same history and the same operations, run once by the real
//! svn and once by this client (history replayed into the fake session from `svnadmin dump`),
//! must leave identical working copies: files, NODES rows, ACTUAL rows and `svn status` output.
//! Skipped when no svn 1.14 or newer is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::oracle::*;
use common::*;
use opensvn_svn_oracle::Oracle;
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth};

#[tokio::test(flavor = "multi_thread")]
async fn checkout_matches_svn_checkout() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_r1(&repo);
    let theirs = repo.checkout("svnwc");
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
    assert_same(&oracle, &theirs, &ours, "checkout");
    // svn itself must accept the working copy we wrote.
    assert_eq!(repo.svn(&ours, &["status"]).trim(), "");
    repo.svn(&ours, &["cleanup"]);
    repo.svn(&ours, &["diff"]);
    assert_eq!(
        repo.svn(&ours, &["info", "--show-item", "revision"]).trim(),
        "1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn update_matches_svn_update() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let theirs = repo.checkout("svnwc");
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

    // Identical local work in both copies.
    for wc in [&theirs, &ours] {
        write(
            &wc.join("docs/guide.txt"),
            "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\n",
        );
        write(&wc.join("README"), "hello\nmine\n");
        write(&wc.join("scratch.txt"), "unversioned\n");
    }
    // r2 from the seed copy: a disjoint edit (clean merge) and an overlapping one (conflict).
    write(
        &seed.join("docs/guide.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
    );
    write(&seed.join("README"), "hello\ntheirs\n");
    write(&seed.join("added.txt"), "x\n");
    repo.svn(&seed, &["add", "-q", "added.txt"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);

    repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
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
    for wc in [&theirs, &ours] {
        if wc == &theirs {
            repo.svn(wc, &["propset", "-q", "color", "green", "docs"]);
            repo.svn(wc, &["propset", "-q", "local", "1", "README"]);
        } else {
            let db = open_db(wc);
            db.set_props(
                &rp("docs"),
                &vec![(b"color".to_vec(), b"green".to_vec())],
                &[],
            )
            .expect("propset");
            db.set_props(
                &rp("README"),
                &vec![(b"local".to_vec(), b"1".to_vec())],
                &[],
            )
            .expect("propset");
        }
    }
    repo.svn(&seed, &["propset", "-q", "color", "blue", "docs"]);
    repo.svn(&seed, &["propset", "-q", "other", "x", "README"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    repo.svn(&theirs, &["update", "-q", "--accept", "postpone"]);
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
    assert_same(&oracle, &theirs, &ours, "property update");
}

/// Runs `local` on both copies, `server` on the seed (r2), then updates both and compares.
async fn scenario(
    name: &str,
    local: impl Fn(&Repo, &Path, bool),
    server: impl Fn(&Repo, &Path),
) -> Option<(Oracle, std::path::PathBuf, std::path::PathBuf)> {
    scenario_at(name, local, server, "", Depth::Unknown).await
}

/// [`scenario`] with the update run on `target` (relative to the working copy root, `""` for
/// the root) at `depth` (`svn update --depth`).
async fn scenario_at(
    name: &str,
    local: impl Fn(&Repo, &Path, bool),
    server: impl Fn(&Repo, &Path),
    target: &str,
    depth: Depth,
) -> Option<(Oracle, std::path::PathBuf, std::path::PathBuf)> {
    let oracle = oracle()?;
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    let theirs = repo.checkout("svnwc");
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
    .expect("checkout r1");
    local(&repo, &theirs, true);
    local(&repo, &ours, false);
    server(&repo, &seed);
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    let mut args = vec!["update", "-q", "--accept", "postpone"];
    let depth_word = depth_arg(depth);
    if let Some(d) = depth_word.as_deref() {
        args.extend(["--depth", d]);
    }
    if !target.is_empty() {
        args.push(target);
    }
    repo.svn(&theirs, &args);
    let path = if target.is_empty() {
        ours.clone()
    } else {
        ours.join(target)
    };
    update(
        &ctx(),
        &repo.fake(None),
        &path,
        None,
        depth,
        &CancelToken::new(),
    )
    .await
    .unwrap_or_else(|e| panic!("{name}: update failed: {e}"));
    // Keep the sandbox and repository alive for the caller.
    std::mem::forget(sb);
    std::mem::forget(repo);
    Some((oracle, theirs, ours))
}

fn mime_binary(repo: &Repo, wc: &Path, is_svn: bool, file: &str) {
    if is_svn {
        repo.svn(
            wc,
            &[
                "propset",
                "-q",
                "svn:mime-type",
                "application/octet-stream",
                file,
            ],
        );
    } else {
        open_db(wc)
            .set_props(
                &rp(file),
                &vec![(
                    b"svn:mime-type".to_vec(),
                    b"application/octet-stream".to_vec(),
                )],
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

#[tokio::test(flavor = "multi_thread")]
async fn missing_items_are_restored_without_server_changes_like_svn() {
    let Some((o, t, w)) = scenario(
        "restore",
        |_, wc, _| {
            std::fs::remove_file(wc.join("run.sh")).expect("rm");
            std::fs::remove_dir_all(wc.join("src")).expect("rm dir");
        },
        |_, seed| write(&seed.join("README"), "hello\nworld\nmore\n"),
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "restore");
}

#[tokio::test(flavor = "multi_thread")]
async fn switch_matches_svn_switch() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = build_r1(&repo);
    // A second line of development next to docs.
    write(
        &seed.join("docs2/guide.txt"),
        "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\n",
    );
    write(&seed.join("docs2/extra.txt"), "extra\n");
    repo.svn(&seed, &["add", "-q", "docs2"]);
    repo.svn(
        &seed,
        &[
            "propset",
            "-q",
            "svn:eol-style",
            "native",
            "docs2/guide.txt",
        ],
    );
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
    let theirs = repo.checkout("svnwc");
    let sb = Sandbox::new();
    let ours = sb.path("wc");
    let fake = repo.fake(None);
    checkout(
        &ctx(),
        &fake,
        &ours,
        Some(svn_types::Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    // svn: switch docs to docs2.
    repo.svn(&theirs, &["update", "-q", "-r1"]);
    let target = svn_types::RepoUrl::parse(&format!("{}/docs2", common::ROOT_URL)).expect("url");
    let svn_url = format!("{}/docs2", repo.url);
    repo.svn(
        &theirs,
        &["switch", "-q", "--ignore-ancestry", "-r2", &svn_url, "docs"],
    );
    svn_client::switch(
        &ctx(),
        &fake,
        &ours.join("docs"),
        &target,
        Some(svn_types::Revnum(2)),
        Depth::Unknown,
        true,
        &CancelToken::new(),
    )
    .await
    .expect("switch");
    assert_same(&oracle, &theirs, &ours, "switch");
}

fn depth_arg(depth: Depth) -> Option<String> {
    match depth {
        Depth::Empty => Some("empty".into()),
        Depth::Files => Some("files".into()),
        Depth::Immediates => Some("immediates".into()),
        Depth::Infinity => Some("infinity".into()),
        _ => None,
    }
}

// ---- review regressions (PR #74) ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_locally_replaced_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replace-vs-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "README"]);
            write(&wc.join("README"), "brand new\n");
            repo.svn(wc, &["add", "-q", "README"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "README"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replace vs delete");
    // The local addition survives as the only row of README.
    let rows: Vec<String> = nodes_snapshot(&w)
        .into_iter()
        .filter(|r| r.starts_with("README|"))
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(
        rows[0].starts_with("README|1|normal|file|None|None|"),
        "{rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_locally_replaced_directory_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replace-dir-vs-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs"]);
            write(&wc.join("docs/mine.txt"), "mine\n");
            repo.svn(wc, &["add", "-q", "docs"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "docs"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replaced dir vs delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_locally_deleted_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "delete-vs-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "README"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "README"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "delete vs delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_prop_edit_of_locally_deleted_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "delete-vs-propedit",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs/guide.txt"]);
        },
        |repo, seed| {
            repo.svn(
                seed,
                &["propset", "-q", "svn:eol-style", "LF", "docs/guide.txt"],
            );
            repo.svn(seed, &["propset", "-q", "color", "red", "docs/guide.txt"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "delete vs prop edit");
    assert!(
        !w.join("docs/guide.txt.prej").exists(),
        "no property reject file for a tree conflict"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_dir_add_over_unversioned_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "dir-obstruction",
        |_, wc, _| write(&wc.join("d/local.txt"), "mine\n"),
        |repo, seed| {
            write(&seed.join("d/x.txt"), "x\n");
            write(&seed.join("d/sub/y.txt"), "y\n");
            repo.svn(seed, &["add", "-q", "d"]);
            repo.svn(seed, &["propset", "-q", "color", "green", "d"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "unversioned dir obstruction");
    assert!(!w.join("d/x.txt").exists(), "the obstruction is left alone");
    let st = svn_status(&o, &w, &[]);
    assert!(st.contains("?       d/local.txt"), "{st}");
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_update_target_keeps_not_present_like_svn() {
    let Some((o, t, w)) = scenario_at(
        "delete-target",
        |_, _, _| {},
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "README"]);
        },
        "README",
        Depth::Unknown,
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "deleted update target");
    assert!(
        nodes_snapshot(&w)
            .iter()
            .any(|r| r.starts_with("README|0|not-present|file|Some(2)")),
        "{:?}",
        nodes_snapshot(&w)
    );
}

/// `svn update --depth D` at r2, then a full update to r3 that deletes a node the narrow
/// update recorded as `not-present`.
async fn depth_case(depth: Depth) {
    let name = format!("depth-{depth:?}");
    let Some((o, t, w)) = scenario_at(
        &name,
        |_, _, _| {},
        |repo, seed| {
            repo.svn(seed, &["update", "-q"]);
            write(&seed.join("README"), "hello\nworld\nagain\n");
            write(&seed.join("new.txt"), "new\n");
            write(&seed.join("newdir/n.txt"), "n\n");
            write(
                &seed.join("docs/guide.txt"),
                "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
            );
            write(&seed.join("docs/extra.txt"), "extra\n");
            repo.svn(seed, &["add", "-q", "new.txt", "newdir", "docs/extra.txt"]);
            repo.svn(seed, &["rm", "-q", "empty.txt"]);
            repo.svn(seed, &["propset", "-q", "color", "blue", "docs"]);
            repo.svn(seed, &["propset", "-q", "top", "1", "."]);
        },
        "",
        depth,
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, &name);
    // The recorded depth never narrows without cropping.
    assert!(
        nodes_snapshot(&w)
            .iter()
            .any(|r| r.starts_with("|0|normal|dir|") && r.contains("Some(\"infinity\")")),
        "{name}: root depth changed: {:?}",
        nodes_snapshot(&w)
    );

    // r3: remove what the narrow update may have marked not-present, then a full update.
    let repo_wc = t.parent().expect("parent").join("seed");
    let mut c = o.svn();
    c.args(["update", "-q"]).current_dir(&repo_wc);
    assert!(c.status().expect("svn").success());
    let mut c = o.svn();
    c.args(["rm", "-q", "new.txt", "newdir/n.txt"])
        .current_dir(&repo_wc);
    assert!(c.status().expect("svn").success());
    let mut c = o.svn();
    c.args(["commit", "-q", "-m", "r3"]).current_dir(&repo_wc);
    assert!(c.status().expect("svn").success());
    let mut c = o.svn();
    c.args(["update", "-q", "--accept", "postpone"])
        .current_dir(&t);
    assert!(c.status().expect("svn").success());
    let repo_dir = t.parent().expect("parent").join("repo");
    let fake = fake_from(&o, &repo_dir);
    update(&ctx(), &fake, &w, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("full update");
    assert_same(&o, &t, &w, &format!("{name} then full update"));
}

/// A fake session replaying the repository at `path`.
fn fake_from(o: &Oracle, path: &Path) -> svn_ra_api::fake::FakeRaSession {
    let mut c = o.svnadmin();
    c.args(["dump", "-q"]).arg(path);
    let out = c.output().expect("dump");
    assert!(out.status.success(), "dump failed");
    let s = session();
    for changes in parse_dump(&out.stdout) {
        s.seed("tester", "replay", &changes).expect("replay");
    }
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn update_depth_empty_matches_svn() {
    depth_case(Depth::Empty).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_depth_files_matches_svn() {
    depth_case(Depth::Files).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_depth_immediates_matches_svn() {
    depth_case(Depth::Immediates).await;
}

// ---- F1 (PR #74 follow-up): BASE moves on under a local delete or replace -------------------

/// After a shadowed tree conflict on `path`: `svn resolve --accept working` on both copies, an
/// unrelated r3, a second update of both, and the comparison. The second update must not raise
/// the conflict again, BASE of `path` must sit at HEAD, and the real svn must be able to commit
/// the (identical) state, i.e. it is not out of date.
async fn resolve_then_update_again(o: &Oracle, t: &Path, w: &Path, path: &str, what: &str) {
    let svn = |cwd: &Path, args: &[&str]| {
        let mut c = o.svn();
        c.args(args).current_dir(cwd);
        let out = c.output().expect("svn");
        assert!(
            out.status.success(),
            "{what}: svn {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    for wc in [t, w] {
        svn(wc, &["resolve", "-q", "--accept", "working", path]);
    }
    assert_same(o, t, w, &format!("{what}: after resolve"));

    let seed = t.parent().expect("parent").join("seed");
    svn(&seed, &["update", "-q"]);
    write(&seed.join("run.sh"), "#!/bin/sh\necho r3\n");
    svn(&seed, &["commit", "-q", "-m", "r3"]);
    svn(t, &["update", "-q", "--accept", "postpone"]);
    let fake = fake_from(o, &t.parent().expect("parent").join("repo"));
    update(&ctx(), &fake, w, None, Depth::Unknown, &CancelToken::new())
        .await
        .unwrap_or_else(|e| panic!("{what}: second update failed: {e}"));
    assert_same(o, t, w, &format!("{what}: second update"));
    let st = svn_status(o, w, &[]);
    assert!(
        !st.lines()
            .any(|l| l.starts_with('C') || l.get(6..7) == Some("C")),
        "{what}: the second update raised a conflict again:\n{st}"
    );
    assert!(
        nodes_snapshot(w)
            .iter()
            .any(|r| r.starts_with(&format!("{path}|0|normal|")) && r.contains("|Some(3)|")),
        "{what}: BASE of {path} is not at HEAD: {:?}",
        nodes_snapshot(w)
    );
    // Same rows as ours: svn commits it without an out-of-date error.
    svn(t, &["commit", "-q", "-m", "commit the local change"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_edit_of_locally_replaced_file_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replace-vs-edit",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "README"]);
            write(&wc.join("README"), "brand new\n");
            repo.svn(wc, &["add", "-q", "README"]);
        },
        |_, seed| write(&seed.join("README"), "hello\nworld\nserver\n"),
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replace vs edit");
    let rows: Vec<String> = nodes_snapshot(&w)
        .into_iter()
        .filter(|r| r.starts_with("README|"))
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows[0].starts_with("README|0|normal|file|Some(2)|"),
        "BASE moves to r2: {rows:?}"
    );
    assert!(
        rows[1].starts_with("README|1|normal|file|None|None|"),
        "the replacement stays: {rows:?}"
    );
    assert_eq!(read(&w.join("README")), "brand new\n");
    resolve_then_update_again(&o, &t, &w, "README", "replace vs edit").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_edit_under_locally_deleted_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "deleted-dir-vs-edit",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs"]);
        },
        |repo, seed| {
            write(
                &seed.join("docs/guide.txt"),
                "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
            );
            write(&seed.join("docs/new.txt"), "new\n");
            write(&seed.join("docs/sub/deeper.txt"), "deeper\n");
            repo.svn(seed, &["add", "-q", "docs/new.txt", "docs/sub"]);
            repo.svn(seed, &["propset", "-q", "color", "blue", "docs"]);
            repo.svn(seed, &["propset", "-q", "shade", "dark", "docs/guide.txt"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "deleted dir vs edit");
    for p in ["docs", "docs/guide.txt"] {
        assert!(
            nodes_snapshot(&w)
                .iter()
                .any(|r| r.starts_with(&format!("{p}|0|normal|")) && r.contains("|Some(2)|")),
            "BASE of {p} moves to r2: {:?}",
            nodes_snapshot(&w)
        );
    }
    assert!(!w.join("docs").exists(), "the local delete stays");
    resolve_then_update_again(&o, &t, &w, "docs", "deleted dir vs edit").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_edit_under_locally_replaced_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replaced-dir-vs-edit",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs"]);
            // The replacement re-adds a file the server edits, and one it also adds.
            write(&wc.join("docs/guide.txt"), "mine\n");
            write(&wc.join("docs/new.txt"), "my new\n");
            repo.svn(wc, &["add", "-q", "docs"]);
        },
        |repo, seed| {
            write(
                &seed.join("docs/guide.txt"),
                "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
            );
            write(&seed.join("docs/new.txt"), "server new\n");
            write(&seed.join("docs/other.txt"), "other\n");
            repo.svn(seed, &["add", "-q", "docs/new.txt", "docs/other.txt"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replaced dir vs edit");
    assert_eq!(read(&w.join("docs/guide.txt")), "mine\n");
    resolve_then_update_again(&o, &t, &w, "docs", "replaced dir vs edit").await;
}

// ---- Shadowed update regressions (PR #74 review of e3fff94) ---------------------------------
//
// Server deletes, adds and edits below a directory that is deleted or replaced locally. The
// drive goes on under the working layer (`shadow`): BASE moves, the working rows and the disk
// stay. Each case compares with svn 1.15 and then resolves and updates again.

#[tokio::test(flavor = "multi_thread")]
async fn incoming_file_delete_under_locally_deleted_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "deleted-dir-vs-file-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "docs/guide.txt"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "deleted dir vs file delete");
    resolve_then_update_again(&o, &t, &w, "docs", "deleted dir vs file delete").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_subdir_delete_under_locally_deleted_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "deleted-dir-vs-subdir-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "src"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "src/lib"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "deleted dir vs subdir delete");
    resolve_then_update_again(&o, &t, &w, "src", "deleted dir vs subdir delete").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_subdir_replaced_by_file_under_locally_deleted_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "deleted-dir-vs-dir-to-file",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "src"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "src/lib"]);
            write(&seed.join("src/lib"), "now a file\n");
            repo.svn(seed, &["add", "-q", "src/lib"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "deleted dir vs dir to file");
    resolve_then_update_again(&o, &t, &w, "src", "deleted dir vs dir to file").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_readded_file_under_replaced_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replaced-dir-vs-file-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs"]);
            write(&wc.join("docs/guide.txt"), "mine\n");
            repo.svn(wc, &["add", "-q", "docs"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "docs/guide.txt"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replaced dir vs file delete");
    assert_eq!(read(&w.join("docs/guide.txt")), "mine\n");
    resolve_then_update_again(&o, &t, &w, "docs", "replaced dir vs file delete").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_of_readded_subdir_under_replaced_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replaced-dir-vs-subdir-delete",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "src"]);
            write(&wc.join("src/lib/deep.rs"), "mine\n");
            write(&wc.join("src/lib/extra.rs"), "extra\n");
            repo.svn(wc, &["add", "-q", "src"]);
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "src/lib"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replaced dir vs subdir delete");
    assert_eq!(read(&w.join("src/lib/deep.rs")), "mine\n");
    resolve_then_update_again(&o, &t, &w, "src", "replaced dir vs subdir delete").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn incoming_edit_of_modified_file_under_replaced_dir_vs_svn() {
    let Some((o, t, w)) = scenario(
        "replaced-dir-vs-edit-of-modified",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "src"]);
            write(&wc.join("src/lib/deep.rs"), "mine\n");
            repo.svn(wc, &["add", "-q", "src"]);
            repo.svn(wc, &["propset", "-q", "loc", "1", "src/lib/deep.rs"]);
            write(&wc.join("src/lib/deep.rs"), "mine2\n");
        },
        |repo, seed| {
            write(&seed.join("src/lib/deep.rs"), "server\n");
            repo.svn(seed, &["propset", "-q", "srv", "1", "src/lib/deep.rs"]);
            repo.svn(seed, &["propset", "-q", "srv", "1", "src/lib"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "replaced dir vs edit of modified");
    assert_eq!(read(&w.join("src/lib/deep.rs")), "mine2\n");
    resolve_then_update_again(&o, &t, &w, "src", "replaced dir vs edit of modified").await;
}

/// The directory is replaced by a copy, so the copied children sit at the copy's op_depth, the
/// same op_depth the shadowed drive uses for its `base-deleted` rows. The server adds
/// `docs/lib`, which the copy already has: `WcTxn::mark_base_deleted_at` must not write over
/// the user's copied row (svn's `db_extend_parent_delete`). A plain `svn add` below a replaced
/// directory puts the children deeper and never reaches that rule; only a copy does.
#[tokio::test(flavor = "multi_thread")]
async fn incoming_add_into_copy_replaced_dir_keeps_copied_rows_vs_svn() {
    let Some((o, t, w)) = scenario(
        "copy-replaced-dir-vs-add",
        |repo, wc, _| {
            repo.svn(wc, &["rm", "-q", "docs"]);
            repo.svn(wc, &["cp", "-q", "src", "docs"]);
            write(&wc.join("docs/lib/deep.rs"), "my copy edit\n");
        },
        |repo, seed| {
            write(&seed.join("docs/lib/deep.rs"), "server\n");
            write(&seed.join("docs/lib/x.rs"), "x\n");
            repo.svn(seed, &["add", "-q", "docs/lib"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "copy-replaced dir vs add");
    let rows = nodes_snapshot(&w);
    for p in ["docs/lib", "docs/lib/deep.rs"] {
        assert!(
            rows.iter()
                .any(|r| r.starts_with(&format!("{p}|1|normal|")) && r.contains("Some(\"src/")),
            "the copied row of {p} stays: {rows:?}"
        );
    }
    assert_eq!(read(&w.join("docs/lib/deep.rs")), "my copy edit\n");
    resolve_then_update_again(&o, &t, &w, "docs", "copy-replaced dir vs add").await;
}

/// Deliberate divergence from svn 1.15. The directory is replaced by a copy, the user edits a
/// copied file, and the server deletes that file. svn deletes the user's modified file from
/// disk (it then shows as missing); we keep it. wc.db is identical, only the disk and therefore
/// `svn status` differ. Losing an uncommitted edit is worse than the mismatch, so keep this
/// behaviour: do not "fix" it to match svn.
#[tokio::test(flavor = "multi_thread")]
async fn incoming_delete_in_copy_replaced_dir_keeps_modified_file_unlike_svn() {
    let Some((o, t, w)) = scenario(
        "copy-replaced-dir-vs-delete",
        |repo, wc, _| {
            repo.svn(wc, &["cp", "-q", "src", "src2"]);
            repo.svn(wc, &["rm", "-q", "src"]);
            repo.svn(wc, &["cp", "-q", "src2", "src"]);
            write(&wc.join("src/lib/deep.rs"), "my copy edit\n");
        },
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "src/lib/deep.rs"]);
        },
    )
    .await
    else {
        return;
    };
    assert_eq!(
        nodes_snapshot(&w),
        nodes_snapshot(&t),
        "NODES rows equal svn's"
    );
    assert_eq!(
        actual_snapshot(&w),
        actual_snapshot(&t),
        "ACTUAL rows equal svn's"
    );
    assert!(
        !t.join("src/lib/deep.rs").exists(),
        "svn 1.15 removes the modified file; if this fails, svn changed and the divergence may be gone"
    );
    assert_eq!(read(&w.join("src/lib/deep.rs")), "my copy edit\n");
    let (mut ours, mut theirs) = (tree_snapshot(&w), tree_snapshot(&t));
    assert!(ours.remove("src/lib/deep.rs").is_some());
    assert!(theirs.remove("src/lib/deep.rs").is_none());
    assert_eq!(ours, theirs, "the rest of the disk equals svn's");
    let st = svn_status(&o, &w, &[]);
    assert!(
        st.lines()
            .any(|l| l.starts_with('M') && l.ends_with("src/lib/deep.rs")),
        "our copy shows the edit as a modification:\n{st}"
    );
}

/// `svn update src/lib` and `svn update docs/guide.txt`: nested targets below the session root.
#[tokio::test(flavor = "multi_thread")]
async fn update_of_nested_targets_matches_svn() {
    for target in ["src/lib", "docs/guide.txt"] {
        let Some((o, t, w)) = scenario_at(
            &format!("nested-{target}"),
            |_, _, _| {},
            |repo, seed| {
                repo.svn(seed, &["update", "-q"]);
                write(&seed.join("src/lib/deep.rs"), "fn deeper() {}\n");
                write(&seed.join("src/lib/new.rs"), "fn new() {}\n");
                write(
                    &seed.join("docs/guide.txt"),
                    "one\ntwo\nthree\nfour\nfive\nsix\nSEVEN\n",
                );
                write(&seed.join("README"), "hello\nchanged\n");
                repo.svn(seed, &["add", "-q", "src/lib/new.rs"]);
            },
            target,
            Depth::Unknown,
        )
        .await
        else {
            return;
        };
        assert_same(&o, &t, &w, target);
    }
}

/// The server replaces a file, a directory by a file and a file by a directory in one revision:
/// each path is deleted and added again by the same drive, so the update must commit the
/// deletion (and remove the old node from disk) before it looks at the added node. Local
/// changes on replaced nodes are covered by `update_replace_kind_vs_svn`.
#[tokio::test(flavor = "multi_thread")]
async fn incoming_replacements_vs_svn() {
    let Some((o, t, w)) = scenario(
        "incoming-replacements",
        |_, _, _| {},
        |repo, seed| {
            repo.svn(seed, &["rm", "-q", "README", "docs", "run.sh"]);
            write(&seed.join("README"), "replaced\n");
            write(&seed.join("docs"), "a file now\n");
            write(&seed.join("run.sh/inner.txt"), "a dir now\n");
            repo.svn(seed, &["add", "-q", "README", "docs", "run.sh"]);
        },
    )
    .await
    else {
        return;
    };
    assert_same(&o, &t, &w, "incoming replacements");
    assert_eq!(
        std::fs::read_to_string(w.join("docs")).unwrap(),
        "a file now\n"
    );
}
