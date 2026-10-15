//! Crash windows of a clean update (see `UpdateSetup::clean`) whose target is an empty directory
//! on disk while wc.db holds working rows below it: a local add of a name the server adds, a
//! made directory the server adds too, local deletes. A failure injected at every transaction
//! and work queue step never leaves the working copy wedged: the next update completes with
//! nothing queued, nothing incomplete and `.svn/tmp` empty, and every queued install has a text.
//! Run with `cargo test -p svn-client --features failpoints`.
#![cfg(feature = "failpoints")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::Path;

use common::*;
use svn_client::local::add::{AddOptions, add};
use svn_client::local::autoprops::AddConfig;
use svn_client::local::changelist::set_changelist;
use svn_client::local::delete::{DeleteOptions, delete};
use svn_client::local::props::{PropSetOptions, propset};
use svn_client::{checkout, update};
use svn_types::{CancelToken, Depth, NodeKind, Revnum};
use svn_wc::db::Presence;

/// r1: `E/x.txt`, `E/y.txt`; r2: `E/x.txt` becomes a directory, the server adds `E/new.txt`
/// (with translation properties), `E/sub/s.txt` and 40 files the drive installs around them.
fn seed() -> svn_ra_api::fake::FakeRaSession {
    let s = session();
    let r1 = vec![
        put("E/x.txt", "x\n"),
        put("E/y.txt", "y\n"),
        put("keep.txt", "k\n"),
    ];
    let mut r2 = vec![
        del("E/x.txt"),
        dir("E/x.txt"),
        put("E/x.txt/in.txt", "in\n"),
        put("E/new.txt", "server new\n"),
        prop("E/new.txt", "svn:eol-style", Some("native")),
        prop("E/new.txt", "svn:executable", Some("*")),
        put("E/sub/s.txt", "s\n"),
    ];
    for i in 0..40 {
        r2.push(put(&format!("E/a/{i:02}.txt"), &format!("{i}\n")));
    }
    s.seed("a", "r1", &r1).expect("seed r1");
    s.seed("a", "r2", &r2).expect("seed r2");
    s
}

/// Deletes `x.txt` and `y.txt`, adds `new.txt` (with a property and a changelist) and the
/// directory `sub` (with an added child), then removes `new.txt` and `sub` from disk.
fn prepare(wc: &Path) {
    let db = open_db(wc);
    db.set_lock_owner(ctx().lock_owner.clone());
    let cancel = CancelToken::new();
    let e = wc.join("E");
    delete(
        &db,
        &cancel,
        &[e.join("x.txt"), e.join("y.txt")],
        &DeleteOptions::default(),
    )
    .expect("delete");
    std::fs::write(e.join("new.txt"), "my new\n").expect("write new.txt");
    std::fs::create_dir_all(e.join("sub")).expect("mkdir sub");
    std::fs::write(e.join("sub/t.txt"), "t\n").expect("write t.txt");
    add(
        &db,
        &cancel,
        &[e.join("new.txt"), e.join("sub")],
        &AddOptions::default(),
        &AddConfig::default(),
    )
    .expect("add");
    propset(
        &db,
        &cancel,
        &rp("E/new.txt"),
        "lp",
        Some(b"local prop"),
        PropSetOptions::default(),
    )
    .expect("propset");
    set_changelist(&db, &cancel, &[e.join("new.txt")], Some("cl"), Depth::Empty)
        .expect("changelist");
    std::fs::remove_file(e.join("new.txt")).expect("rm new.txt");
    std::fs::remove_dir_all(e.join("sub")).expect("rm sub");
    assert_eq!(std::fs::read_dir(&e).expect("read E").count(), 0);
}

async fn update_e(s: &svn_ra_api::fake::FakeRaSession, wc: &Path) -> svn_types::Result<()> {
    update(
        &ctx(),
        s,
        &wc.join("E"),
        Some(Revnum(2)),
        Depth::Unknown,
        &CancelToken::new(),
    )
    .await
    .map(|_| ())
}

/// Temp files left in `.svn/tmp`, lock markers aside (see `update_replace_kind_failpoints`).
fn tmp_entries(wc: &Path) -> Vec<String> {
    match std::fs::read_dir(wc.join(".svn/tmp")) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with("opensvn-lock-"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every queued install from the pristine store names a node whose top layer has a text.
fn assert_installable_queue(wc: &Path, what: &str) {
    let db = open_db(wc);
    for (_, item) in db.work_items().expect("queue") {
        if let Ok(svn_wc::workqueue::WorkItem::FileInstall {
            relpath,
            source: None,
            ..
        }) = svn_wc::workqueue::WorkItem::from_skel(&item)
        {
            let node = db.read_node(&rp(&relpath)).expect("read").expect("node");
            assert!(node.checksum.is_some(), "{what}: install of {relpath}");
        }
    }
}

/// The update reached r2 and nothing is queued, incomplete or left in `.svn/tmp`. The local
/// add of `new.txt` and the made directory `sub` stay as working layers. When the failed drive
/// already committed part of `E`, the target is no longer empty and the next update is not a
/// clean one: it reads wc.db and meets the local adds as tree conflicts (svn does the same).
fn assert_complete(wc: &Path, what: &str) {
    let db = open_db(wc);
    assert!(db.work_items().expect("queue").is_empty(), "{what}: queue");
    let rows = base_rows(&db);
    // A failed drive can leave a directory it added incomplete after it recorded the tree
    // conflict of the local delete it replaces (`x.txt`); the next update skips a conflicted
    // node, as svn does, so that directory waits for the conflict to be resolved.
    let incomplete: Vec<_> = rows
        .iter()
        .filter(|r| r.1 == "incomplete")
        .filter(|r| db.conflict(&rp(&r.0)).expect("read").is_none())
        .collect();
    assert!(
        incomplete.is_empty(),
        "{what}: incomplete rows {incomplete:?}"
    );
    let rd = |p: &str| {
        std::fs::read_to_string(wc.join(p)).unwrap_or_else(|e| panic!("{what}: read {p}: {e}"))
    };
    assert_eq!(rd("E/a/39.txt"), "39\n", "{what}");
    for (path, kind) in [("E/new.txt", NodeKind::File), ("E/sub", NodeKind::Dir)] {
        let top = db.read_node(&rp(path)).expect("read").expect("top");
        assert_eq!(
            (top.op_depth, top.presence, top.kind),
            (2, Presence::Normal, kind),
            "{what}: the local add of {path} stays"
        );
        let base = db.read_base(&rp(path)).expect("read");
        if base.is_none() {
            assert!(
                db.conflict(&rp(path)).expect("read").is_some(),
                "{what}: {path} neither updated nor conflicted"
            );
        }
    }
    if db.read_base(&rp("E/new.txt")).expect("read").is_some() {
        assert_eq!(rd("E/new.txt"), "server new\n", "{what}");
    }
    if db.read_base(&rp("E/sub/s.txt")).expect("read").is_some() {
        let s = db
            .read_node(&rp("E/sub/s.txt"))
            .expect("read")
            .expect("s.txt");
        assert_eq!(
            (s.op_depth, s.presence),
            (2, Presence::BaseDeleted),
            "{what}: s.txt is deleted in the local layer of sub"
        );
    }
    assert_eq!(tmp_entries(wc), Vec::<String>::new(), "{what}: .svn/tmp");
}

// One test function: failpoint configuration is process global.
#[tokio::test(flavor = "multi_thread")]
async fn clean_update_below_working_rows_survives_a_failure_at_every_step() {
    let scenario = fail::FailScenario::setup();
    let sb = Sandbox::new();
    let mut runs = 0;
    for point in [
        "wc.txn.before_commit",
        "wc.txn.after_commit",
        "wc.wq.after_delete",
        "wc.wq.after_fileinfo",
        "update.after_delete_txn",
        "update.after_merge3_before_txn",
        "update.after_txn_before_queue",
        "pristine.install.before_rename",
    ] {
        // As in `update_replace_kind_failpoints`, the lock obtain's own commit window is skipped.
        let first_n = usize::from(point == "wc.txn.after_commit");
        for n in first_n.. {
            let what = format!("{point} #{n}");
            let s = seed();
            let wc = sb.path(&format!("{point}-{n}"));
            checkout(
                &ctx(),
                &s,
                &wc,
                Some(Revnum(1)),
                Depth::Infinity,
                false,
                &CancelToken::new(),
            )
            .await
            .expect("checkout");
            prepare(&wc);
            fail::cfg(point, &format!("{n}*off->1*return->off")).expect("cfg");
            let first = update_e(&s, &wc).await;
            fail::remove(point);
            runs += 1;
            assert_installable_queue(&wc, &what);
            if first.is_ok() {
                assert_complete(&wc, &what);
                break;
            }
            update_e(&s, &wc)
                .await
                .unwrap_or_else(|e| panic!("{what}: resumed update failed: {e}"));
            assert_complete(&wc, &what);
            assert!(n < 500, "{what}: the failpoint never stops firing");
        }
    }
    assert!(runs > 16, "only {runs} runs");
    scenario.teardown();
}
