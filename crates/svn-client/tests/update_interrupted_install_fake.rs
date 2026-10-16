//! A file another client installed just before it was killed, in a directory its update left
//! incomplete. svn 1.15 renames the working file into place inside the transaction that adds its
//! BASE row (`install_working_file` in `svn_wc__db_base_add_file`), so a kill between the rename
//! and the commit leaves the file on disk without its row. The update that finishes the drive
//! must adopt that file, not turn it into an "unversioned, incoming add" tree conflict with a
//! `base-deleted` row: the working copy then equals a fresh checkout.
//!
//! The state is built directly (checkout, then the row of one file removed and its directories
//! marked `incomplete`), so it does not depend on where a kill lands.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::*;
use svn_client::{StatusOptions, UpdateAction, checkout, update};
use svn_ra_api::fake::FakeRaSession;
use svn_types::{CancelToken, Depth, Revnum};
use svn_wc::conflicts::{ConflictSkel, TreeReason};
use svn_wc::db::Presence;

async fn checked_out(s: &FakeRaSession) -> (Sandbox, std::path::PathBuf) {
    let sb = Sandbox::new();
    let wc = sb.path("wc");
    checkout(
        &ctx(),
        s,
        &wc,
        None,
        Depth::Infinity,
        false,
        &CancelToken::new(),
    )
    .await
    .expect("checkout");
    (sb, wc)
}

fn seed(s: &FakeRaSession) {
    s.seed(
        "a",
        "r1",
        &[
            put("README", "top\n"),
            put("d/a.txt", "a\n"),
            // A fixed eol-style keeps its line endings in repository-normal form.
            put("d/b.txt", "line one\r\nline two\r\n$Rev$\r\n"),
            prop("d/b.txt", "svn:eol-style", Some("CRLF")),
            prop("d/b.txt", "svn:keywords", Some("Rev")),
            put("d/c.txt", "c\n"),
        ],
    )
    .expect("seed");
}

/// What a killed `svn checkout` leaves once `svn cleanup` ran: `file` is on disk without a BASE
/// row and `dirs` are `incomplete`.
fn interrupt_after_install(wc: &Path, file: &str, dirs: &[&str]) {
    let db = open_db(wc);
    db.with_write(|tx| {
        let n = tx
            .execute("DELETE FROM nodes WHERE local_relpath = ?1", [file])
            .unwrap();
        assert_eq!(n, 1, "{file}");
        for d in dirs {
            let n = tx
                .execute(
                    "UPDATE nodes SET presence = 'incomplete' \
                     WHERE local_relpath = ?1 AND op_depth = 0",
                    [d],
                )
                .unwrap();
            assert_eq!(n, 1, "{d}");
        }
        Ok(())
    })
    .expect("rewrite rows");
}

async fn continue_at_r1(s: &FakeRaSession, wc: &Path) -> svn_client::UpdateSummary {
    update(
        &ctx(),
        s,
        wc,
        Some(Revnum(1)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .expect("continue")
}

fn working_rows(wc: &Path) -> Vec<(String, i64, String)> {
    open_db(wc)
        .with_read(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT local_relpath, op_depth, presence FROM nodes WHERE op_depth > 0 \
                     ORDER BY local_relpath",
                )
                .expect("prepare");
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .expect("query");
            Ok(rows.map(|r| r.expect("row")).collect())
        })
        .expect("read")
}

async fn local_changes(wc: &Path) -> Vec<String> {
    svn_client::status(&ctx(), wc, &StatusOptions::default(), &CancelToken::new())
        .await
        .expect("status")
        .iter()
        .map(|s| format!("{} {:?}", s.path, s.text))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_installed_without_its_row_is_adopted_when_the_drive_resumes() {
    for (file, dirs) in [
        ("README", &[""][..]),
        ("d/a.txt", &["", "d"][..]),
        // Translated on disk (CRLF, expanded keyword): compared in repository-normal form.
        ("d/b.txt", &["", "d"][..]),
    ] {
        let s = session();
        seed(&s);
        let (_sb, wc) = checked_out(&s).await;
        let before = std::fs::read(wc.join(file)).expect("read");
        let fresh = open_db(&wc)
            .read_base(&rp(file))
            .expect("read")
            .expect("node");
        interrupt_after_install(&wc, file, dirs);

        let sum = continue_at_r1(&s, &wc).await;
        assert!(
            sum.events
                .iter()
                .any(|e| e.path.as_str() == file && e.action == UpdateAction::Existed),
            "{file}: {:?}",
            sum.events
        );
        let db = open_db(&wc);
        assert!(
            working_rows(&wc).is_empty(),
            "{file}: {:?}",
            working_rows(&wc)
        );
        assert!(db.conflict(&rp(file)).expect("read").is_none(), "{file}");
        assert!(db.work_items().expect("queue").is_empty());
        let node = db.read_base(&rp(file)).expect("read").expect("node");
        assert_eq!(node.presence, Presence::Normal);
        assert_eq!(node.revision, Some(1));
        assert_eq!(node.checksum, fresh.checksum, "{file}");
        // Recorded like a fresh install, so status takes the fast path.
        assert_eq!(node.translated_size, fresh.translated_size, "{file}");
        assert!(node.last_mod_time.is_some(), "{file}");
        assert!(
            base_rows(&db).iter().all(|r| r.1 == "normal" && r.3 == 1),
            "{file}: {:?}",
            base_rows(&db)
        );
        assert_eq!(std::fs::read(wc.join(file)).expect("read"), before);
        assert!(local_changes(&wc).await.is_empty(), "{file}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_different_file_in_the_way_of_a_resumed_drive_stays_a_tree_conflict() {
    let s = session();
    seed(&s);
    let (_sb, wc) = checked_out(&s).await;
    interrupt_after_install(&wc, "d/a.txt", &["", "d"]);
    std::fs::write(wc.join("d/a.txt"), "mine\n").expect("write");

    continue_at_r1(&s, &wc).await;
    // The local file is never replaced: BASE gets the server's file under a `base-deleted`
    // layer, as for any unversioned obstruction.
    assert_eq!(read(&wc.join("d/a.txt")), "mine\n");
    let db = open_db(&wc);
    let rec = db
        .conflict(&rp("d/a.txt"))
        .expect("read")
        .expect("conflict");
    let skel = ConflictSkel::from_skel(&rec.skel).expect("skel");
    assert_eq!(skel.tree().expect("tree").reason, TreeReason::Unversioned);
    assert_eq!(
        working_rows(&wc),
        [("d/a.txt".to_owned(), 2, "base-deleted".to_owned())]
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_in_place_of_a_resumed_file_stays_a_tree_conflict() {
    let s = session();
    s.seed("a", "r1", &[put("d/a.txt", "link x")])
        .expect("seed");
    let (_sb, wc) = checked_out(&s).await;
    interrupt_after_install(&wc, "d/a.txt", &["", "d"]);
    std::fs::remove_file(wc.join("d/a.txt")).expect("rm");
    std::os::unix::fs::symlink("x", wc.join("d/a.txt")).expect("symlink");

    continue_at_r1(&s, &wc).await;
    assert!(
        std::fs::symlink_metadata(wc.join("d/a.txt"))
            .expect("stat")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        working_rows(&wc),
        [("d/a.txt".to_owned(), 2, "base-deleted".to_owned())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_identical_file_in_the_way_of_a_plain_add_stays_a_tree_conflict() {
    // Outside an incomplete directory the file was not put there by an interrupted update:
    // svn's rule (an unversioned obstruction) applies even when the text is the same.
    let s = session();
    s.seed("a", "r1", &[put("a.txt", "a\n")]).expect("seed");
    let (_sb, wc) = checked_out(&s).await;
    std::fs::write(wc.join("new.txt"), "same\n").expect("write");
    s.seed("a", "r2", &[put("new.txt", "same\n")])
        .expect("seed");
    update(&ctx(), &s, &wc, None, Depth::Unknown, &CancelToken::new())
        .await
        .expect("update");
    assert_eq!(
        working_rows(&wc),
        [("new.txt".to_owned(), 1, "base-deleted".to_owned())]
    );
}
