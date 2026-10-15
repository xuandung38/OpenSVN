//! Checkout and update against the in-memory repository.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use common::*;
use svn_client::{UpdateAction, checkout, update};
use svn_types::{CancelToken, Depth, Revnum};

#[tokio::test(flavor = "multi_thread")]
async fn checkout_materializes_tree_and_records_base() {
    let s = session();
    s.seed(
        "alice",
        "initial",
        &[
            put("trunk/a.txt", "alpha\n"),
            put("trunk/src/lib.rs", "pub fn f() {}\n"),
            put("README", "hello\n"),
            prop("README", "svn:eol-style", Some("native")),
        ],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let sum = checkout(
        &ctx(),
        &s,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    assert_eq!(sum.revision, Some(Revnum(1)));
    assert_eq!(read(&wc.join("trunk/a.txt")), "alpha\n");
    assert_eq!(read(&wc.join("trunk/src/lib.rs")), "pub fn f() {}\n");
    assert_eq!(read(&wc.join("README")), "hello\n");

    let db = open_db(&wc);
    let rows = base_rows(&db);
    assert!(
        rows.iter().all(|r| r.1 == "normal" && r.3 == 1),
        "rows: {rows:?}"
    );
    assert_eq!(rows.len(), 6, "rows: {rows:?}");
    assert!(db.work_items().expect("queue").is_empty());
    assert!(
        db.wc_lock_held(&rp(""))
            .map(|s| s == svn_wc::db::LockState::Free)
            .unwrap_or(false)
    );
    let readme = db.read_base(&rp("README")).expect("read").expect("node");
    assert!(readme.checksum.is_some());
    assert_eq!(readme.repos_relpath.as_deref(), Some("README"));
    assert!(sum.events.iter().any(|e| e.action == UpdateAction::Add));
}

#[tokio::test(flavor = "multi_thread")]
async fn update_applies_server_changes_and_bumps_untouched_files() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("a.txt", "one\n"),
            put("b.txt", "two\n"),
            put("d/c.txt", "three\n"),
        ],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");

    s.seed(
        "a",
        "r2",
        &[
            put("a.txt", "one\nmore\n"),
            put("e.txt", "new\n"),
            del("d/c.txt"),
            dir("n/m"),
        ],
    )
    .expect("seed");
    let sum = update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert_eq!(sum.revision, Some(Revnum(2)));
    assert_eq!(read(&wc.join("a.txt")), "one\nmore\n");
    assert_eq!(read(&wc.join("e.txt")), "new\n");
    assert!(!wc.join("d/c.txt").exists());
    assert!(wc.join("n/m").is_dir());

    let db = open_db(&wc);
    let rows = base_rows(&db);
    // Every remaining node, touched or not, is at r2 (b.txt was not part of the drive).
    assert!(
        rows.iter().all(|r| r.3 == 2 && r.1 == "normal"),
        "rows: {rows:?}"
    );
    let names: Vec<_> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(names, ["", "a.txt", "b.txt", "d", "e.txt", "n", "n/m"]);
    assert!(db.work_items().expect("queue").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn update_keeps_unversioned_files_inside_a_deleted_directory() {
    let s = session();
    s.seed("a", "r1", &[put("d/c.txt", "x\n")]).expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    std::fs::write(wc.join("d/notes.txt"), "mine\n").expect("write");
    s.seed("a", "r2", &[del("d")]).expect("seed");
    update(&ctx(), &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect("update");
    assert!(!wc.join("d/c.txt").exists());
    assert_eq!(read(&wc.join("d/notes.txt")), "mine\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_update_is_resumed_by_the_next_update() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("a.txt", "a\n"),
            put("b.txt", "b\n"),
            put("d/c.txt", "c\n"),
        ],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        &s,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    s.seed(
        "a",
        "r2",
        &[
            put("a.txt", "a2\n"),
            put("b.txt", "b2\n"),
            put("d/c.txt", "c2\n"),
            put("d/new.txt", "n\n"),
        ],
    )
    .expect("seed");

    // Cancel as soon as the first node is applied.
    let cancel = CancelToken::new();
    let mut c = ctx();
    let trip = cancel.clone();
    c.progress = Some(std::sync::Arc::new(move |_ev: &svn_client::UpdateEvent| {
        trip.cancel()
    }));
    let err = update(&c, &s, &wc, None, Depth::Unknown, &cancel)
        .await
        .expect_err("cancelled");
    assert_eq!(err.code, svn_types::ErrorCode::Cancelled, "{err}");
    {
        let db = open_db(&wc);
        let root = db.read_base(&rp("")).expect("read").expect("root");
        assert_eq!(root.presence, svn_wc::db::Presence::Incomplete);
        // Work that the cancelled drive queued may still be queued (like svn, whose queue run
        // stops on cancellation): the next update runs the queue first.
        assert_eq!(
            db.wc_lock_held(&rp("")).expect("lock"),
            svn_wc::db::LockState::Free,
            "lock released on cancellation"
        );
    }

    let sum = update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("resume");
    assert_eq!(sum.revision, Some(Revnum(2)));
    for (f, text) in [
        ("a.txt", "a2\n"),
        ("b.txt", "b2\n"),
        ("d/c.txt", "c2\n"),
        ("d/new.txt", "n\n"),
    ] {
        assert_eq!(read(&wc.join(f)), text, "{f}");
    }
    let db = open_db(&wc);
    assert!(db.work_items().expect("queue").is_empty());
    let rows = base_rows(&db);
    assert!(
        rows.iter().all(|r| r.1 == "normal" && r.3 == 2),
        "rows: {rows:?}"
    );
}

/// A nested target whose parent is switched cannot be anchored at the session URL: both an
/// update and a switch of it are refused before anything changes.
#[tokio::test(flavor = "multi_thread")]
async fn nested_target_below_a_switched_parent_is_refused_for_update_and_switch() {
    let s = session();
    s.seed(
        "a",
        "r1",
        &[
            put("trunk/src/lib/a.rs", "trunk\n"),
            put("branches/b/src/lib/a.rs", "branch\n"),
            put("branches/b/other/x.rs", "other\n"),
        ],
    )
    .expect("seed");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    let cancel = CancelToken::new();
    checkout(&ctx(), &s, &wc, None, Depth::Infinity, false, &cancel)
        .await
        .expect("checkout");
    let url = |rel: &str| svn_types::RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url");
    svn_client::switch(
        &ctx(),
        &s,
        &wc.join("trunk/src"),
        &url("branches/b/src"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("switch trunk/src");
    assert_eq!(read(&wc.join("trunk/src/lib/a.rs")), "branch\n");

    let err = update(
        &ctx(),
        &s,
        &wc.join("trunk/src/lib"),
        None,
        Depth::Unknown,
        &cancel,
    )
    .await
    .expect_err("update below a switched parent");
    assert_eq!(err.code, svn_types::ErrorCode::WcInvalidOperation, "{err}");
    let err = svn_client::switch(
        &ctx(),
        &s,
        &wc.join("trunk/src/lib"),
        &url("branches/b/other"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect_err("switch below a switched parent");
    assert_eq!(err.code, svn_types::ErrorCode::WcInvalidOperation, "{err}");
    assert!(err.to_string().contains("switched"), "{err}");
    assert_eq!(
        read(&wc.join("trunk/src/lib/a.rs")),
        "branch\n",
        "nothing changed"
    );
}

/// `svn switch`'s ancestry check: a location with no history in common with the working copy is
/// refused (E195012) before anything changes, a copy of it is taken, and `ignore_ancestry`
/// takes anything in the repository. A folder switched on its own is checked too.
#[tokio::test(flavor = "multi_thread")]
async fn switch_refuses_unrelated_locations_unless_ancestry_is_ignored() {
    let s = session();
    s.seed("a", "r1", &[put("trunk/src/a.rs", "trunk\n")])
        .expect("seed");
    s.seed(
        "a",
        "r2",
        &[svn_ra_api::fake::FakeChange::Copy {
            from: rp("trunk"),
            rev: Revnum(1),
            to: rp("branches/b"),
        }],
    )
    .expect("branch");
    s.seed("a", "r3", &[put("other/src/a.rs", "other\n")])
        .expect("unrelated");
    let at_trunk = s
        .at(&svn_types::RepoUrl::parse(&format!("{ROOT_URL}/trunk")).expect("url"))
        .expect("session at trunk");
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
    std::fs::write(wc.join("src/a.rs"), "trunk\nlocal\n").expect("edit");
    let url = |rel: &str| svn_types::RepoUrl::parse(&format!("{ROOT_URL}/{rel}")).expect("url");
    let before = base_rows(&open_db(&wc));

    for target in [wc.clone(), wc.join("src")] {
        let what = if target == wc { "other" } else { "other/src" };
        let err = svn_client::switch(
            &ctx(),
            &at_trunk,
            &target,
            &url(what),
            None,
            Depth::Unknown,
            false,
            &cancel,
        )
        .await
        .expect_err("unrelated switch");
        assert_eq!(err.code, svn_types::ErrorCode::UnrelatedResources, "{err}");
        assert_eq!(err.svn_code, Some(195_012), "{err}");
        assert_eq!(base_rows(&open_db(&wc)), before, "nothing changed");
        assert_eq!(read(&wc.join("src/a.rs")), "trunk\nlocal\n");
    }

    svn_client::switch(
        &ctx(),
        &at_trunk,
        &wc,
        &url("branches/b"),
        None,
        Depth::Unknown,
        false,
        &cancel,
    )
    .await
    .expect("switch to the branch");
    let db = open_db(&wc);
    assert_eq!(
        db.read_base(&rp("src"))
            .expect("base")
            .and_then(|n| n.repos_relpath)
            .as_deref(),
        Some("branches/b/src")
    );
    assert_eq!(read(&wc.join("src/a.rs")), "trunk\nlocal\n");

    svn_client::switch(
        &ctx(),
        &s.at(&url("branches/b")).expect("session at the branch"),
        &wc,
        &url("other"),
        None,
        Depth::Unknown,
        true,
        &cancel,
    )
    .await
    .expect("switch ignoring ancestry");
    assert_eq!(
        open_db(&wc)
            .read_base(&rp("src"))
            .expect("base")
            .and_then(|n| n.repos_relpath)
            .as_deref(),
        Some("other/src")
    );
}
