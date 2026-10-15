//! A switch whose connection fails mid-drive is resumed like an update: the target already
//! points to the new URL, so the resumed drive is anchored at it and reports the folders the
//! broken drive did not reach with `link_path`; the end of the drive moves whatever the server
//! had no change for. A switch that cannot be resumed leaves a working copy that the same switch
//! finishes, local edits included.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

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

/// `trunk` and `branches/b` with the same layout and different texts, except `README` (the
/// same in both, so no drive changes it).
fn seeded() -> svn_ra_api::fake::FakeRaSession {
    let s = session();
    let mut changes = vec![
        put("trunk/README", "readme\n"),
        put("branches/b/README", "readme\n"),
    ];
    for d in 0..3 {
        for f in 0..4 {
            changes.push(put(
                &format!("trunk/d{d}/f{f}.txt"),
                &format!("trunk {d}.{f}\n"),
            ));
            changes.push(put(
                &format!("branches/b/d{d}/f{f}.txt"),
                &format!("branch {d}.{f}\n"),
            ));
        }
    }
    s.seed("alice", "r1", &changes).expect("seed");
    s
}

fn url(path: &str) -> RepoUrl {
    RepoUrl::parse(&format!("{ROOT_URL}/{path}")).unwrap()
}

async fn checkout_trunk(s: &svn_ra_api::fake::FakeRaSession, wc: &std::path::Path) {
    checkout(
        &ctx(),
        &s.at(&url("trunk")).unwrap(),
        wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
}

/// The working copy is entirely at `branches/b` r1: texts, BASE rows (every node where its
/// place in the tree puts it, nothing switched, nothing incomplete), no leftover work or lock.
fn assert_switched(wc: &std::path::Path) {
    for d in 0..3 {
        for f in 0..4 {
            assert_eq!(
                read(&wc.join(format!("d{d}/f{f}.txt"))),
                format!("branch {d}.{f}\n"),
                "d{d}/f{f}.txt"
            );
        }
    }
    let db = open_db(wc);
    let rows = base_rows(&db);
    assert_eq!(rows.len(), 1 + 1 + 3 + 12, "rows: {rows:?}");
    for (relpath, presence, _, rev, repos) in &rows {
        let expected = if relpath.is_empty() {
            "branches/b".to_owned()
        } else {
            format!("branches/b/{relpath}")
        };
        assert_eq!(repos, &expected, "{relpath}: {rows:?}");
        assert_eq!((presence.as_str(), *rev), ("normal", 1), "{relpath}");
    }
    assert!(db.work_items().expect("queue").is_empty());
    assert_eq!(
        db.wc_lock_held(&rp("")).expect("lock"),
        svn_wc::db::LockState::Free
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switch_broken_mid_drive_resumes_at_the_switched_target() {
    let s = seeded();
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout_trunk(&s, &wc).await;
    std::fs::write(wc.join("README"), "local edit\n").unwrap();
    let ra = RecRa::new(s.at(&url("trunk")).unwrap());
    ra.update_breaks
        .lock()
        .unwrap()
        .extend([Some((5, broken())), None]);
    let sum = switch(
        &ctx(),
        &ra,
        &wc,
        &url("branches/b"),
        Some(Revnum(1)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("switch resumed");
    assert_eq!(sum.revision, Some(Revnum(1)));
    assert_switched(&wc);
    assert_eq!(read(&wc.join("README")), "local edit\n");
    let log = ra.log.lock().unwrap().clone();
    assert!(
        log.iter()
            .any(|l| l == &format!("reparented {ROOT_URL}/branches/b")),
        "{log:?}"
    );
    // The resumed report: the root is where the session points, the folders the broken
    // drive had not reached are described at their old location.
    let reports = ra.reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(reports[1][0].starts_with("set_path  r1"), "{reports:?}");
    assert!(
        reports[1]
            .iter()
            .any(|l| l.starts_with("link_path ") && l.contains("/trunk/")),
        "{reports:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switch_that_cannot_be_resumed_is_finished_by_the_same_switch() {
    let s = seeded();
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout_trunk(&s, &wc).await;
    std::fs::write(wc.join("README"), "local edit\n").unwrap();
    std::fs::write(wc.join("d2/f3.txt"), "edited before the switch\n").unwrap();
    let ra = RecRa::new(s.at(&url("trunk")).unwrap());
    // The answer always breaks before the same file: the first drive gets the folders before
    // it, the resumes bring nothing new.
    *ra.update_break_at.lock().unwrap() = Some(("d2/f2.txt".to_owned(), broken()));
    let err = switch(
        &ctx(),
        &ra,
        &wc,
        &url("branches/b"),
        Some(Revnum(1)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect_err("a switch that never gets past the cut");
    assert_eq!(
        err.connection_failure(),
        Some(ConnectionFailure::TlsClosedUncleanly)
    );
    {
        // Half-switched and resumable: the root points to the branch and stays incomplete,
        // nothing is queued or locked, local edits are untouched.
        let db = open_db(&wc);
        let rows = base_rows(&db);
        let root = rows.iter().find(|r| r.0.is_empty()).unwrap();
        assert_eq!(root.4, "branches/b", "{rows:?}");
        assert!(rows.iter().any(|r| r.1 == "incomplete"), "{rows:?}");
        assert!(db.work_items().expect("queue").is_empty());
        assert_eq!(
            db.wc_lock_held(&rp("")).expect("lock"),
            svn_wc::db::LockState::Free
        );
    }
    assert_eq!(read(&wc.join("README")), "local edit\n");
    assert_eq!(read(&wc.join("d2/f3.txt")), "edited before the switch\n");

    // What "Continue switch" does: the same switch, over a session at the working copy's URL
    // (now the branch), once the connection works.
    switch(
        &ctx(),
        &s.at(&url("branches/b")).unwrap(),
        &wc,
        &url("branches/b"),
        Some(Revnum(1)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("the same switch finishes");
    assert_eq!(read(&wc.join("README")), "local edit\n");
    let db = open_db(&wc);
    let rows = base_rows(&db);
    for (relpath, presence, _, rev, repos) in &rows {
        let expected = if relpath.is_empty() {
            "branches/b".to_owned()
        } else {
            format!("branches/b/{relpath}")
        };
        assert_eq!(repos, &expected, "{relpath}: {rows:?}");
        assert_eq!((presence.as_str(), *rev), ("normal", 1), "{relpath}");
    }
    for d in 0..2 {
        for f in 0..4 {
            assert_eq!(
                read(&wc.join(format!("d{d}/f{f}.txt"))),
                format!("branch {d}.{f}\n")
            );
        }
    }
    // The locally edited file got the branch's change merged into a conflict or kept the
    // edit: either way the local text is not lost.
    assert!(
        read(&wc.join("d2/f3.txt")).contains("edited before the switch"),
        "{}",
        read(&wc.join("d2/f3.txt"))
    );
}

/// Like svn 1.15 (`bump_node_revision` with a new repository path), a switch moves everything
/// below its target where the target's new location puts it, a folder switched elsewhere
/// before included, even when the server had no change to send for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_moves_a_folder_switched_below_it_like_svn() {
    let s = seeded();
    s.seed(
        "alice",
        "r2",
        &[
            put("branches/x/f0.txt", "branch 0.0\n"),
            put("branches/x/f1.txt", "branch 0.1\n"),
            put("branches/x/f2.txt", "branch 0.2\n"),
            put("branches/x/f3.txt", "branch 0.3\n"),
        ],
    )
    .expect("r2");
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout_trunk(&s, &wc).await;
    let ra = s.at(&url("trunk")).unwrap();
    switch(
        &ctx(),
        &ra,
        &wc.join("d0"),
        &url("branches/x"),
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("switch d0");
    let d0 = |db: &svn_wc::db::WcDb| {
        base_rows(db)
            .into_iter()
            .find(|r| r.0 == "d0")
            .map(|r| r.4)
            .unwrap()
    };
    assert_eq!(d0(&open_db(&wc)), "branches/x");
    // `branches/x` has the texts of `branches/b/d0`: nothing to send for d0.
    switch(
        &ctx(),
        &ra,
        &wc,
        &url("branches/b"),
        None,
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("switch the root");
    let db = open_db(&wc);
    assert_eq!(d0(&db), "branches/b/d0");
    for (relpath, _, _, rev, repos) in base_rows(&db) {
        let expected = if relpath.is_empty() {
            "branches/b".to_owned()
        } else {
            format!("branches/b/{relpath}")
        };
        assert_eq!((repos, rev), (expected, 2), "{relpath}");
    }
}
