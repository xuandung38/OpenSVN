//! Commit compared with svn 1.15: the same local changes are committed once by the real svn (to
//! a `file://` repository) and once by this client (to the fake session replaying that
//! repository's history). Both working copies must end identical (files, executable and read-only
//! bits, NODES and ACTUAL rows, `svn status`), and fresh checkouts of both repositories must match
//! too, so the server received the same tree.
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::oracle::*;
use common::*;
use svn_client::checkout;
use svn_client::commit::{CommitContext, CommitOptions, commit};
use svn_client::local::{self, AddConfig, AddOptions, DeleteOptions, PropSetOptions};
use svn_ra_api::RaSession;
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, MemoryLockOwnership};
use svn_wc::db::{LockOwner, WcDb};

/// Our side: a working copy checked out by this client from the replayed history.
struct Ours {
    wc: PathBuf,
    db: WcDb,
    fake: FakeRaSession,
    state: PathBuf,
    cancel: CancelToken,
    _sb: Sandbox,
}

async fn ours_from(repo: &Repo) -> Ours {
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let fake = repo.fake(None);
    let cancel = CancelToken::new();
    checkout(&ctx(), &fake, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    let db = WcDb::open(&wc).expect("open");
    db.set_lock_owner(LockOwner {
        store: Arc::new(MemoryLockOwnership::for_current_process("test-boot")),
        pid: std::process::id(),
        boot_id: "test-boot".to_owned(),
    });
    let state = sb.path("state");
    Ours {
        wc,
        db,
        fake,
        state,
        cancel,
        _sb: sb,
    }
}

impl Ours {
    fn add(&self, rels: &[&str]) {
        let paths: Vec<PathBuf> = rels.iter().map(|r| self.wc.join(r)).collect();
        local::add(
            &self.db,
            &self.cancel,
            &paths,
            &AddOptions::default(),
            &AddConfig::default(),
        )
        .expect("add");
    }

    fn rm(&self, rels: &[&str]) {
        let paths: Vec<PathBuf> = rels.iter().map(|r| self.wc.join(r)).collect();
        local::delete(&self.db, &self.cancel, &paths, &DeleteOptions::default()).expect("rm");
    }

    fn propset(&self, rel: &str, name: &str, value: &str) {
        local::propset(
            &self.db,
            &self.cancel,
            &rp(rel),
            name,
            Some(value.as_bytes()),
            PropSetOptions::default(),
        )
        .expect("propset");
    }

    async fn commit(&self, msg: &str) -> u64 {
        let res = commit(
            &self.db,
            &CommitContext {
                ra: &self.fake,
                state_dir: &self.state,
                cancel: &self.cancel,
            },
            std::slice::from_ref(&self.wc),
            &CommitOptions::new(msg),
        )
        .await
        .expect("commit")
        .expect("something to commit");
        assert!(
            self.db.work_items().expect("wq").is_empty(),
            "post-commit work finished"
        );
        res.revision.0
    }
}

fn svn_commit(repo: &Repo, wc: &Path, msg: &str) -> u64 {
    let out = repo.svn(wc, &["commit", "-m", msg]);
    let line = out
        .lines()
        .find_map(|l| l.strip_prefix("Committed revision "))
        .unwrap_or_else(|| panic!("no revision in {out:?}"));
    line.trim_end_matches('.').parse().expect("revision")
}

fn read_only(path: &Path) -> bool {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .readonly()
}

/// Both repositories hold the same tree: every file has the same repository-normal text and
/// properties (`svn cat` / `svn proplist` against the fake session's `get_file`), and fresh
/// checkouts by svn and by this client have the same rows, directory properties and status.
///
/// The fake session sends no `svn:entry:committed-rev`, so a checkout from it expands `$Rev$`
/// as `-1`; working texts of files with `svn:keywords` are therefore compared through the
/// repository-normal check only.
async fn assert_same_repository(oracle: &opensvn_svn_oracle::Oracle, repo: &Repo, ours: &Ours) {
    let listing = repo.svn(repo.base().as_path(), &["list", "-R", &repo.url]);
    let mut keyword_files = Vec::new();
    for entry in listing.lines().filter(|l| !l.ends_with('/')) {
        let url = format!("{}/{entry}", repo.url);
        let mut cat = oracle.svn();
        cat.args(["cat", "--ignore-keywords", &url]);
        let theirs_text = cat.output().expect("svn cat").stdout;
        let mut buf: Vec<u8> = Vec::new();
        let meta = ours
            .fake
            .get_file(&rp(entry), None, &mut buf, &ours.cancel)
            .await
            .expect("get_file");
        assert_eq!(buf, theirs_text, "repository text of '{entry}'");
        let mut ours_props: Vec<String> = meta
            .props
            .iter()
            .filter(|p| !p.name.starts_with("svn:entry:") && !p.name.starts_with("svn:wc:"))
            .map(|p| {
                format!(
                    "{}={}",
                    p.name,
                    String::from_utf8_lossy(p.value.as_deref().unwrap_or_default())
                )
            })
            .collect();
        ours_props.sort();
        let names = repo.svn(repo.base().as_path(), &["proplist", "-q", &url]);
        let mut theirs_props: Vec<String> = names
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| {
                let v = repo.svn(repo.base().as_path(), &["propget", "--strict", n, &url]);
                format!("{n}={v}")
            })
            .collect();
        theirs_props.sort();
        assert_eq!(ours_props, theirs_props, "properties of '{entry}'");
        if theirs_props.iter().any(|p| p.starts_with("svn:keywords=")) {
            keyword_files.push(entry.to_owned());
        }
    }

    let theirs = repo.checkout(&format!("verify-{}", std::process::id()));
    let sb = Sandbox::new();
    let fresh = sb.path("verify");
    checkout(
        &ctx(),
        &ours.fake,
        &fresh,
        None,
        Depth::Infinity,
        false,
        &ours.cancel,
    )
    .await
    .expect("verify checkout");
    let without_keywords = |root: &Path| {
        let mut t = tree_snapshot(root);
        for k in &keyword_files {
            t.remove(k);
        }
        t
    };
    assert_eq!(
        without_keywords(&fresh),
        without_keywords(&theirs),
        "fresh checkouts: working files differ"
    );
    // NODES rows too, without the keyword-expanded working size of keyword files.
    let nodes = |root: &Path| -> Vec<String> {
        nodes_snapshot(root)
            .into_iter()
            .map(|row| {
                let mut f: Vec<&str> = row.split('|').collect();
                if keyword_files.iter().any(|k| k == f[0]) && f.len() > 8 {
                    f[8] = "KW";
                }
                f.join("|")
            })
            .collect()
    };
    assert_eq!(nodes(&fresh), nodes(&theirs), "fresh checkouts: NODES");
    assert_eq!(
        actual_snapshot(&fresh),
        actual_snapshot(&theirs),
        "fresh checkouts: ACTUAL"
    );
    std::fs::remove_dir_all(&theirs).expect("rm verify");
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_matches_svn_commit() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let ours = ours_from(&repo).await;
    assert_same(&oracle, &theirs, &ours.wc, "before commit");

    // Identical local work: text edit, keywords, CRLF under eol-style=native (rewritten to LF by
    // the post-commit work item), executable and needs-lock bits, directory properties, adds and
    // deletes of files and directories.
    for wc in [&theirs, &ours.wc] {
        write(&wc.join("README"), "hello\nbrave\nworld\n");
        write(&wc.join("kw.txt"), "rev $Rev$ end\nsame $Rev: 99 $ too\n");
        write(&wc.join("crlf.txt"), "a\r\nb\r\n");
        write(&wc.join("tool.sh"), "#!/bin/sh\n");
        write(&wc.join("new/deep/leaf.txt"), "leaf\n");
    }
    repo.svn(
        &theirs,
        &["add", "-q", "kw.txt", "crlf.txt", "tool.sh", "new"],
    );
    ours.add(&["kw.txt", "crlf.txt", "tool.sh", "new"]);
    for (rel, name, value) in [
        ("kw.txt", "svn:keywords", "Rev"),
        ("crlf.txt", "svn:eol-style", "native"),
        ("tool.sh", "svn:executable", "*"),
        ("run.sh", "svn:needs-lock", "*"),
        ("docs", "color", "blue"),
        ("new/deep", "depth", "two"),
    ] {
        repo.svn(&theirs, &["propset", "-q", name, value, rel]);
        ours.propset(rel, name, value);
    }
    repo.svn(&theirs, &["rm", "-q", "empty.txt", "src/lib"]);
    ours.rm(&["empty.txt", "src/lib"]);
    assert_same(&oracle, &theirs, &ours.wc, "local changes");

    let their_rev = svn_commit(&repo, &theirs, "r2");
    let our_rev = ours.commit("r2").await;
    assert_eq!(our_rev, their_rev);
    assert_same(&oracle, &theirs, &ours.wc, "after commit");
    assert_eq!(
        read(&ours.wc.join("kw.txt")),
        "rev $Rev: 2 $ end\nsame $Rev: 2 $ too\n"
    );
    assert_eq!(read(&ours.wc.join("crlf.txt")), "a\nb\n");
    assert_eq!(
        read_only(&ours.wc.join("run.sh")),
        read_only(&theirs.join("run.sh")),
        "svn:needs-lock makes the committed file read-only"
    );
    assert!(read_only(&ours.wc.join("run.sh")));
    assert_eq!(repo.svn(&ours.wc, &["status"]), "");
    assert_same_repository(&oracle, &repo, &ours).await;

    // A second commit on top: the keyword file changes again, the committed directory gets a
    // child and a property goes away.
    for wc in [&theirs, &ours.wc] {
        write(&wc.join("kw.txt"), "rev $Rev: 2 $ end\nmore\n");
        write(&wc.join("new/second.txt"), "second\n");
    }
    repo.svn(&theirs, &["add", "-q", "new/second.txt"]);
    ours.add(&["new/second.txt"]);
    repo.svn(&theirs, &["propdel", "-q", "color", "docs"]);
    local::propset(
        &ours.db,
        &ours.cancel,
        &rp("docs"),
        "color",
        None,
        PropSetOptions::default(),
    )
    .expect("propdel");
    let their_rev = svn_commit(&repo, &theirs, "r3");
    let our_rev = ours.commit("r3").await;
    assert_eq!(our_rev, their_rev);
    assert_same(&oracle, &theirs, &ours.wc, "after second commit");
    assert_eq!(read(&ours.wc.join("kw.txt")), "rev $Rev: 3 $ end\nmore\n");
    assert_same_repository(&oracle, &repo, &ours).await;

    // svn accepts the working copy we committed from.
    repo.svn(&ours.wc, &["cleanup"]);
    assert_eq!(repo.svn(&ours.wc, &["diff"]), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_of_copy_and_move_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build_r1(&repo);
    let theirs = repo.checkout("svnwc");
    let ours = ours_from(&repo).await;

    repo.svn(&theirs, &["copy", "-q", "README", "README.copy"]);
    local::copy(
        &ours.db,
        &ours.cancel,
        &ours.wc.join("README"),
        &ours.wc.join("README.copy"),
    )
    .expect("copy");
    repo.svn(&theirs, &["move", "-q", "docs/guide.txt", "docs/moved.txt"]);
    local::move_path(
        &ours.db,
        &ours.cancel,
        &ours.wc.join("docs/guide.txt"),
        &ours.wc.join("docs/moved.txt"),
    )
    .expect("move");
    for wc in [&theirs, &ours.wc] {
        write(&wc.join("README.copy"), "hello\ncopied\n");
    }
    assert_same(&oracle, &theirs, &ours.wc, "local copy and move");

    let their_rev = svn_commit(&repo, &theirs, "r2");
    let our_rev = ours.commit("r2").await;
    assert_eq!(our_rev, their_rev);
    assert_same(&oracle, &theirs, &ours.wc, "after commit");
    assert_eq!(repo.svn(&ours.wc, &["status"]), "");
    assert_same_repository(&oracle, &repo, &ours).await;
}

/// Issue #111: changes below a switched subtree and outside it, committed together. svn anchors
/// the commit at the common ancestor of the changes; through a session at the working copy URL
/// the commit is refused before any server transaction, and `commit_anchor` names the anchor.
#[tokio::test(flavor = "multi_thread")]
async fn commit_with_a_switched_subtree_matches_svn() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    let seed = repo.checkout("seed");
    write(&seed.join("trunk/top.txt"), "top\n");
    write(&seed.join("trunk/sub/x.txt"), "trunk x\n");
    write(&seed.join("branches/b/x.txt"), "branch x\n");
    repo.svn(&seed, &["add", "-q", "trunk", "branches"]);
    repo.svn(&seed, &["commit", "-q", "-m", "r1"]);
    let trunk_url = format!("{}/trunk", repo.url);
    let branch_url = format!("{}/branches/b", repo.url);

    // svn: trunk checkout, sub switched to the branch.
    let theirs = repo.base().join("svnwc");
    repo.svn(
        &repo.base(),
        &["checkout", "-q", &trunk_url, theirs.to_str().unwrap()],
    );
    repo.svn(
        &theirs,
        &["switch", "-q", "--ignore-ancestry", &branch_url, "sub"],
    );

    // Ours: the same through the fake session.
    let fake = repo.fake(None);
    let root = fake.repos_root().clone();
    let at_trunk = fake.at(&root.join(&rp("trunk")).unwrap()).unwrap();
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(
        &ctx(),
        &at_trunk,
        &wc,
        None,
        Depth::Infinity,
        false,
        &cancel,
    )
    .await
    .expect("checkout");
    svn_client::switch(
        &ctx(),
        &at_trunk,
        &wc.join("sub"),
        &root.join(&rp("branches/b")).unwrap(),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("switch");
    assert_same(&oracle, &theirs, &wc, "after switch");

    for w in [&theirs, &wc] {
        write(&w.join("top.txt"), "top\nedited\n");
        write(&w.join("sub/x.txt"), "branch x\nedited\n");
        write(&w.join("sub/new.txt"), "new on the branch\n");
    }
    repo.svn(&theirs, &["add", "-q", "sub/new.txt"]);
    let db = WcDb::open(&wc).expect("open");
    db.set_lock_owner(LockOwner {
        store: Arc::new(MemoryLockOwnership::for_current_process("test-boot")),
        pid: std::process::id(),
        boot_id: "test-boot".to_owned(),
    });
    local::add(
        &db,
        &cancel,
        &[wc.join("sub/new.txt")],
        &AddOptions::default(),
        &AddConfig::default(),
    )
    .expect("add");
    let theirs_rev = svn_commit(&repo, &theirs, "both sides");

    let state = sb.path("state");
    let targets = [wc.clone()];
    let at_trunk_opts = CommitOptions {
        session_prefix: "trunk".into(),
        ..CommitOptions::new("both sides")
    };
    // Through the working copy URL: refused, nothing sent, nothing changed.
    let before = nodes_snapshot(&wc);
    let err = commit(
        &db,
        &CommitContext {
            ra: &at_trunk,
            state_dir: &state,
            cancel: &cancel,
        },
        &targets,
        &at_trunk_opts,
    )
    .await
    .expect_err("outside the session");
    assert_eq!(err.code, svn_types::ErrorCode::WcInvalidOperation, "{err}");
    assert_eq!(nodes_snapshot(&wc), before);
    assert_eq!(
        fake.youngest_rev(&cancel).await.unwrap().0,
        1,
        "no revision was made"
    );

    // svn's anchor: the common ancestor of trunk/top.txt and branches/b/...: the root.
    let anchor = svn_client::commit::commit_anchor(&db, &targets, &at_trunk_opts, "trunk")
        .expect("anchor")
        .expect("changes");
    assert_eq!(anchor, "");
    let ours_rev = commit(
        &db,
        &CommitContext {
            ra: &fake,
            state_dir: &state,
            cancel: &cancel,
        },
        &targets,
        &CommitOptions::new("both sides"),
    )
    .await
    .expect("commit")
    .expect("something to commit")
    .revision
    .0;
    assert_eq!(ours_rev, theirs_rev);
    drop(db);
    assert_same(&oracle, &theirs, &wc, "after commit");
    let ours = Ours {
        wc: wc.clone(),
        db: WcDb::open(&wc).expect("open"),
        fake,
        state,
        cancel,
        _sb: sb,
    };
    assert_same_repository(&oracle, &repo, &ours).await;
}
