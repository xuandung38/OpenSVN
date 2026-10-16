//! A checkout interrupted by one client is finished by the other, compared with a fresh
//! `svn checkout -r R` of svn 1.15 (working files, NODES and ACTUAL rows, `svn status`):
//!
//! - ours → svn: our checkout loses the connection mid-drive and gives up waiting for it; the
//!   working copy is left incomplete at R, without a lock, and a plain `svn update -r R`
//!   finishes it.
//! - svn → ours: `svn checkout` is killed mid-drive (a real interruption, retried until one
//!   leaves incomplete folders); after the `svn cleanup` svn itself asks for, our Continue (an
//!   update to the revision the incomplete folders record) finishes it. A kill can land between
//!   the rename of a working file and the commit of its BASE row (svn 1.15 installs the file
//!   inside that transaction); the last test builds that state on purpose.
//!
//! The fake RA replays the svn repository's history under its own URL and UUID, so the
//! working copy's repository row is rewritten when it changes hands (what `svn relocate` would
//! do between two URLs of the same repository).
//!
//! Skipped when no svn 1.15 is installed (fails instead when `OPENSVN_REQUIRE_SVN=1`).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
#[path = "support/rec_ra.rs"]
mod rec_ra;

use std::path::Path;
use std::time::{Duration, Instant};

use common::oracle::*;
use common::*;
use rec_ra::RecRa;
use svn_client::netwait::NetworkWait;
use svn_client::{checkout, update};
use svn_types::{CancelToken, ConnectionFailure, Depth, ErrorCode, Revnum, SvnError};

const DIRS: usize = 24;
const FILES: usize = 40;

/// r1: `DIRS` folders of `FILES` files (imported in one commit); r2 changes one file, so HEAD
/// moves past the revision the interrupted checkout was reaching.
fn build(repo: &Repo) {
    let tree = repo.base().join("import");
    for d in 0..DIRS {
        for f in 0..FILES {
            write(
                &tree.join(format!("d{d:02}/f{f:02}.txt")),
                &format!("{d}.{f}\n{}\n", "x".repeat(200)),
            );
        }
    }
    write(&tree.join("README"), "top\n");
    repo.svn(
        &repo.base(),
        &[
            "import",
            "-q",
            "-m",
            "r1",
            tree.to_str().unwrap(),
            &repo.url,
        ],
    );
    let seed = repo.checkout("seed");
    write(&seed.join("d00/f00.txt"), "changed in r2\n");
    repo.svn(&seed, &["commit", "-q", "-m", "r2"]);
}

/// Points the working copy's repository row at `root` / `uuid`.
fn relocate(wc: &Path, root: &str, uuid: &str) {
    let db = open_db(wc);
    db.with_write(|tx| {
        tx.execute("UPDATE repository SET root = ?1, uuid = ?2", [root, uuid])
            .expect("relocate");
        Ok(())
    })
    .expect("relocate");
}

fn repo_uuid(repo: &Repo) -> String {
    repo.svn(
        &repo.base(),
        &["info", "--show-item", "repos-uuid", &repo.url],
    )
    .trim()
    .to_owned()
}

/// `(relpath, revision)` of the incomplete BASE rows.
fn incomplete(wc: &Path) -> Vec<(String, i64)> {
    base_rows(&open_db(wc))
        .into_iter()
        .filter(|r| r.1 == "incomplete")
        .map(|r| (r.0, r.3))
        .collect()
}

/// The revision the app's Continue uses: the highest one on an incomplete folder.
fn resume_rev(wc: &Path) -> u64 {
    let rev = incomplete(wc)
        .iter()
        .map(|(_, r)| *r)
        .max()
        .expect("incomplete");
    u64::try_from(rev).unwrap()
}

fn reference(repo: &Repo, name: &str, rev: u64) -> std::path::PathBuf {
    let wc = repo.base().join(name);
    repo.svn(
        &repo.base(),
        &[
            "checkout",
            "-q",
            "-r",
            &rev.to_string(),
            &repo.url,
            wc.to_str().unwrap(),
        ],
    );
    wc
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_we_left_incomplete_is_finished_by_svn_update() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let sb = Sandbox::new();
    let ours = sb.path("ours");
    let ra = RecRa::new(repo.fake(None));
    ra.update_breaks.lock().unwrap().extend([Some((
        300,
        SvnError::new(ErrorCode::Timeout, "operation timed out")
            .with_connection(ConnectionFailure::Timeout),
    ))]);
    // The network does not come back within the wait.
    *ra.probe_default.lock().unwrap() =
        Some(SvnError::new(ErrorCode::Network, "dns error").with_unreachable());
    let mut c = ctx();
    c.network_wait = Some(NetworkWait {
        max_wait: Duration::from_millis(100),
        initial_delay: Duration::from_millis(10),
        ..NetworkWait::default()
    });
    let err = checkout(
        &c,
        &ra,
        &ours,
        Some(Revnum(1)),
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect_err("gave up waiting");
    assert!(err.is_connection_class(), "{err:?}");
    assert_eq!(resume_rev(&ours), 1);
    assert_eq!(
        open_db(&ours).wc_lock_held(&rp("")).expect("lock"),
        svn_wc::db::LockState::Free
    );

    relocate(&ours, &repo.url, &repo_uuid(&repo));
    // svn finishes it at the revision it was reaching, not at HEAD (r2).
    repo.svn(&ours, &["update", "-q", "-r", "1"]);
    let theirs = reference(&repo, "theirs-a", 1);
    assert_same(&oracle, &theirs, &ours, "ours finished by svn update");
    assert!(incomplete(&ours).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_svn_left_incomplete_is_finished_by_our_continue() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let uuid = repo_uuid(&repo);
    // Kill `svn checkout -r 1` part-way; retry with a later kill until one leaves incomplete
    // folders behind (a kill before wc.db exists, or after the end, does not).
    let mut theirs_left = None;
    for attempt in 0..40u32 {
        let wc = repo.base().join(format!("killed-{attempt}"));
        let mut cmd = oracle.svn();
        cmd.args(["checkout", "-q", "-r", "1", &repo.url])
            .arg(&wc)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("svn checkout");
        let started = Instant::now();
        let delay = Duration::from_millis(5 * u64::from(attempt % 20));
        while !wc.join(".svn/wc.db").exists() && started.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(delay);
        let _ = child.kill();
        let _ = child.wait();
        // A kill before the schema was written leaves an empty database: not this case.
        let opened = svn_wc::WcDb::open(&wc).is_ok();
        if opened && !incomplete(&wc).is_empty() {
            theirs_left = Some(wc);
            break;
        }
    }
    let Some(left) = theirs_left else {
        panic!("svn checkout never stopped with incomplete folders");
    };
    // svn's own rule after a killed client: cleanup first (its lock has no owner to check).
    repo.svn(&left, &["cleanup"]);
    assert!(
        !incomplete(&left).is_empty(),
        "cleanup kept the incomplete folders"
    );
    let rev = resume_rev(&left);
    assert_eq!(rev, 1);

    // Our Continue: an update to the revision the incomplete folders record.
    relocate(&left, common::ROOT_URL, "uuid-demo");
    update(
        &ctx(),
        &repo.fake(None),
        &left,
        Some(Revnum(rev)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("continue");
    relocate(&left, &repo.url, &uuid);
    let theirs = reference(&repo, "theirs-b", rev);
    assert_same(
        &oracle,
        &theirs,
        &left,
        "svn's checkout finished by our continue",
    );
    assert!(incomplete(&left).is_empty());
    // And svn agrees there is nothing left to do.
    let st = svn_status(&oracle, &left, &[]);
    assert!(st.is_empty(), "{st}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_svn_installed_without_its_row_is_adopted_by_our_continue() {
    let Some(oracle) = oracle() else { return };
    let repo = Repo::new(&oracle);
    build(&repo);
    let uuid = repo_uuid(&repo);
    // What a kill between svn's rename of `d05/f03.txt` and the commit of its row leaves: the
    // file on disk, no row, the folders on its way `incomplete` (no timing involved).
    let left = reference(&repo, "left", 1);
    open_db(&left)
        .with_write(|tx| {
            tx.execute("DELETE FROM nodes WHERE local_relpath = 'd05/f03.txt'", [])
                .expect("drop row");
            tx.execute(
                "UPDATE nodes SET presence = 'incomplete' \
                 WHERE local_relpath IN ('', 'd05') AND op_depth = 0",
                [],
            )
            .expect("incomplete");
            Ok(())
        })
        .expect("interrupt");
    assert!(left.join("d05/f03.txt").is_file());
    assert_eq!(resume_rev(&left), 1);

    relocate(&left, common::ROOT_URL, "uuid-demo");
    update(
        &ctx(),
        &repo.fake(None),
        &left,
        Some(Revnum(1)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("continue");
    relocate(&left, &repo.url, &uuid);
    let theirs = reference(&repo, "theirs-c", 1);
    assert_same(
        &oracle,
        &theirs,
        &left,
        "an install without its row, adopted",
    );
    assert!(incomplete(&left).is_empty());
    let st = svn_status(&oracle, &left, &[]);
    assert!(st.is_empty(), "{st}");
}
