//! WC_LOCK semantics, ownership recovery and interplay with a real svn process.
#![allow(clippy::expect_used, clippy::panic)]

mod schema_support;

use std::path::Path;
use std::sync::Arc;

use schema_support::*;
use svn_types::{Depth, ErrorCode, LockOwnershipStore, MemoryLockOwnership, NodeKind, RelPath};
use svn_wc::db::{BaseNode, LockOwner, LockState, NewWc, ObtainMode, Presence, WcDb, sql_err};
use svn_wc::skel::Skel;

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("relpath")
}

fn base_dir(path: &str) -> BaseNode {
    BaseNode {
        kind: NodeKind::Dir,
        presence: Presence::Normal,
        repos_root_url: "http://h/r".into(),
        repos_uuid: "u".into(),
        repos_relpath: path.into(),
        revision: 1,
        changed_rev: Some(1),
        changed_date: None,
        changed_author: None,
        depth: Some(Depth::Infinity),
        properties: None,
        checksum: None,
        symlink_target: None,
        translated_size: None,
        last_mod_time: None,
        inherited_props: None,
        dav_cache: None,
    }
}

/// WC with directories a, a/b, c.
fn wc(dir: &Path) -> WcDb {
    let db = WcDb::create(
        dir,
        &NewWc {
            repos_root_url: "http://h/r".into(),
            repos_uuid: "u".into(),
            repos_relpath: String::new(),
            revision: 1,
            depth: Depth::Infinity,
        },
    )
    .expect("create");
    for p in ["a", "a/b", "c"] {
        db.base_add(&rp(p), &base_dir(p), &[]).expect("dir");
    }
    db
}

fn owner(store: &Arc<MemoryLockOwnership>, pid: u32) -> LockOwner {
    LockOwner {
        store: store.clone(),
        pid,
        boot_id: "boot".into(),
    }
}

#[test]
fn levels_obtain_release_and_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = wc(tmp.path());
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));

    // levels 0 covers only the directory itself.
    db.wc_lock_obtain(&rp("a"), 0, ObtainMode::Normal)
        .expect("lock a");
    assert_eq!(
        db.wc_lock_held(&rp("a")).expect("held"),
        LockState::HeldByUs
    );
    assert_eq!(db.wc_lock_held(&rp("a/b")).expect("held"), LockState::Free);
    db.wc_lock_obtain(&rp("a/b"), 0, ObtainMode::Normal)
        .expect("lock a/b");
    assert!(
        db.wc_lock_obtain(&rp("a"), 0, ObtainMode::Normal).is_err(),
        "already locked via itself"
    );
    db.wc_lock_release(&rp("a/b")).expect("release");
    db.wc_lock_release(&rp("a")).expect("release");
    assert!(db.wc_lock_release(&rp("a")).is_err(), "not locked");

    // Infinite lock covers the subtree and blocks overlapping attempts.
    db.wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
        .expect("lock a");
    assert_eq!(
        db.wc_lock_held(&rp("a/b")).expect("held"),
        LockState::HeldByUs
    );
    let err = db
        .wc_lock_obtain(&rp("a/b"), 0, ObtainMode::Normal)
        .expect_err("covered");
    assert_eq!(err.code, ErrorCode::WcBusy);
    db.wc_lock_obtain(&rp("c"), 0, ObtainMode::Normal)
        .expect("unrelated dir");
    assert!(
        store
            .owned_by_dead_process(&tmp.path().canonicalize().expect("c"))
            .expect("dead")
            .is_empty()
    );
    db.wc_lock_release(&rp("c")).expect("release");
    db.wc_lock_release(&rp("a")).expect("release");

    // A held lock may be extended upward over its own lock.
    db.wc_lock_obtain(&rp("a/b"), 0, ObtainMode::Normal)
        .expect("lock a/b");
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("extend to root");
    db.wc_lock_release(&RelPath::root()).expect("release");
    db.wc_lock_release(&rp("a/b")).expect("release");
    let rows: i64 = db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM wc_lock", [], |r| r.get(0))
                .map_err(sql_err)
        })
        .expect("rows");
    assert_eq!(rows, 0);
}

#[test]
fn rejects_missing_nodes_pending_work_and_missing_owner() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = wc(tmp.path());
    assert!(
        db.wc_lock_obtain(&rp("a"), 0, ObtainMode::Normal).is_err(),
        "no owner registered"
    );
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    let err = db
        .wc_lock_obtain(&rp("ghost"), 0, ObtainMode::Normal)
        .expect_err("ghost");
    assert_eq!(err.code, ErrorCode::PathNotFound);
    assert!(db.wc_lock_obtain(&rp("a"), -2, ObtainMode::Normal).is_err());

    db.queue_work(&[Skel::parse(b"(file-install x)").expect("skel")])
        .expect("queue");
    let err = db
        .wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
        .expect_err("pending work");
    assert_eq!(err.code, ErrorCode::WcNeedsCleanup);
    db.wc_lock_obtain(&rp("a"), -1, ObtainMode::Cleanup)
        .expect("cleanup may lock");
}

#[test]
fn foreign_rows_block_and_are_never_removed_without_steal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = wc(tmp.path());
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    // A row written by svn / TortoiseSVN: no ownership record.
    db.with_write(|tx| {
        tx.execute(
            "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (1, 'a', -1)",
            [],
        )
        .map_err(sql_err)?;
        Ok(())
    })
    .expect("foreign row");
    assert_eq!(
        db.wc_lock_held(&rp("a/b")).expect("held"),
        LockState::HeldByOther
    );
    assert_eq!(
        db.wc_lock_obtain(&rp("a/b"), 0, ObtainMode::Normal)
            .expect_err("busy")
            .code,
        ErrorCode::WcBusy
    );
    assert_eq!(
        db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
            .expect_err("busy")
            .code,
        ErrorCode::WcBusy
    );
    db.wc_lock_obtain(&rp("c"), 0, ObtainMode::Normal)
        .expect("c is free");
    db.wc_lock_release(&rp("c")).expect("release");

    assert!(
        db.wc_cleanup_locks().expect("cleanup").recovered.is_empty(),
        "cleanup leaves foreign rows"
    );
    assert_eq!(
        db.wc_lock_held(&rp("a")).expect("held"),
        LockState::HeldByOther
    );
    assert!(db.wc_lock_steal(&rp("a")).expect("steal"));
    assert!(!db.wc_lock_steal(&rp("a")).expect("steal again"));
    assert_eq!(db.wc_lock_held(&rp("a")).expect("held"), LockState::Free);
}

#[test]
fn crashed_app_process_locks_are_recovered_but_live_ones_are_not() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_dead = dead.clone();
    let store = Arc::new(MemoryLockOwnership::new(Arc::new(
        move |pid: u32, _: &str| {
            !(pid == 111 && probe_dead.load(std::sync::atomic::Ordering::SeqCst))
        },
    )));
    {
        let crashing = wc(tmp.path());
        crashing.set_lock_owner(owner(&store, 111));
        crashing
            .wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
            .expect("lock");
        // Handle dropped without release = process crash.
    }
    let db = WcDb::open(tmp.path()).expect("reopen");
    db.set_lock_owner(owner(&store, 222));
    assert_eq!(
        db.wc_lock_held(&rp("a")).expect("held"),
        LockState::HeldByOther
    );
    assert!(
        db.wc_cleanup_locks().expect("cleanup").recovered.is_empty(),
        "owner 111 is still alive"
    );
    assert_eq!(
        db.wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
            .expect_err("busy")
            .code,
        ErrorCode::WcBusy
    );

    dead.store(true, std::sync::atomic::Ordering::SeqCst);
    // The refused attempt above must not have clobbered the record of the dead owner.
    assert_eq!(
        db.wc_cleanup_locks().expect("cleanup").recovered,
        vec![rp("a")]
    );
    assert_eq!(db.wc_lock_held(&rp("a")).expect("held"), LockState::Free);
    db.wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
        .expect("lock after recovery");
}

#[test]
fn rows_of_unversioned_directories_without_our_traces_are_left_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = wc(tmp.path());
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    db.with_write(|tx| {
        tx.execute("INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (1, 'gone', 0), (1, 'c', 0)", [])
            .map_err(sql_err)?;
        Ok(())
    })
    .expect("rows");
    let report = db.wc_cleanup_locks().expect("cleanup");
    assert_eq!(report, svn_wc::db::LockCleanup::default());
    for d in ["gone", "c"] {
        assert_eq!(
            db.wc_lock_held(&rp(d)).expect("held"),
            LockState::HeldByOther,
            "{d}"
        );
    }
}

/// Simulates what `svn cleanup` does to our traces (marker files vanish, rows are cleared) and a
/// later svn operation that locks the same directory: the dead record alone must not be enough
/// to delete that lock.
#[test]
fn dead_record_without_marker_never_deletes_a_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_dead = dead.clone();
    let store = Arc::new(MemoryLockOwnership::new(Arc::new(
        move |pid: u32, _: &str| {
            !(pid == 111 && probe_dead.load(std::sync::atomic::Ordering::SeqCst))
        },
    )));
    {
        let crashing = wc(tmp.path());
        crashing.set_lock_owner(owner(&store, 111));
        crashing
            .wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
            .expect("lock");
    }
    // `svn cleanup`: marker files and rows are gone ...
    for entry in std::fs::read_dir(tmp.path().join(".svn/tmp")).expect("tmp") {
        std::fs::remove_file(entry.expect("entry").path()).expect("rm");
    }
    let db = WcDb::open(tmp.path()).expect("reopen");
    db.with_write(|tx| {
        tx.execute("DELETE FROM wc_lock", []).map_err(sql_err)?;
        // ... and a new svn operation takes the very same row.
        tx.execute(
            "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (1, '', -1)",
            [],
        )
        .map_err(sql_err)?;
        Ok(())
    })
    .expect("svn lock");
    db.set_lock_owner(owner(&store, 222));
    dead.store(true, std::sync::atomic::Ordering::SeqCst);

    let report = db.wc_cleanup_locks().expect("cleanup");
    assert!(report.recovered.is_empty());
    assert_eq!(report.needs_user_action, vec![RelPath::root()]);
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther
    );
    let err = db
        .wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect_err("busy");
    assert_eq!(err.code, ErrorCode::WcBusy);
    assert!(err.message.contains("cleanup"), "{}", err.message);
    // Only an explicit decision removes it.
    assert!(db.wc_lock_steal(&RelPath::root()).expect("steal"));
}

#[test]
fn open_and_recover_clears_locks_of_dead_owners() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_dead = dead.clone();
    let store = Arc::new(MemoryLockOwnership::new(Arc::new(
        move |pid: u32, _: &str| {
            !(pid == 111 && probe_dead.load(std::sync::atomic::Ordering::SeqCst))
        },
    )));
    {
        let crashing = wc(tmp.path());
        crashing.set_lock_owner(owner(&store, 111));
        crashing
            .wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
            .expect("lock");
    }
    dead.store(true, std::sync::atomic::Ordering::SeqCst);
    let (db, report) = WcDb::open_and_recover(tmp.path(), owner(&store, 222)).expect("open");
    assert_eq!(report.recovered, vec![rp("a")]);
    assert_eq!(db.wc_lock_held(&rp("a")).expect("held"), LockState::Free);
    let markers = std::fs::read_dir(tmp.path().join(".svn/tmp"))
        .expect("tmp")
        .count();
    assert_eq!(markers, 0, "markers of recovered locks are removed");
}

#[test]
fn racing_obtains_keep_the_winners_ownership_intact() {
    for _ in 0..10 {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_dead = dead.clone();
        let store = Arc::new(MemoryLockOwnership::new(Arc::new(
            move |pid: u32, _: &str| {
                !(pid == 111 && probe_dead.load(std::sync::atomic::Ordering::SeqCst))
            },
        )));
        {
            let racing = wc(tmp.path());
            racing.set_lock_owner(owner(&store, 111));
            let results: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = (0..4)
                    .map(|_| s.spawn(|| racing.wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("thread"))
                    .collect()
            });
            assert_eq!(
                results.iter().filter(|r| r.is_ok()).count(),
                1,
                "{results:?}"
            );
            // Handle dropped: crash with the winner holding the lock.
        }
        dead.store(true, std::sync::atomic::Ordering::SeqCst);
        let db = WcDb::open(tmp.path()).expect("reopen");
        db.set_lock_owner(owner(&store, 222));
        assert_eq!(
            db.wc_cleanup_locks().expect("cleanup").recovered,
            vec![rp("a")]
        );
    }
}

#[test]
fn format_32_working_copies_cannot_be_locked() {
    if skip_without_svn_1_15() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let w = checkout(&base, &url, "w", &["--compatible-version=1.15"]);
    let db = WcDb::open(&w).expect("open");
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    let err = db
        .wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect_err("ro");
    assert_eq!(err.code, ErrorCode::WcFormatUnsupported);
}

#[test]
fn svn_refuses_to_run_write_commands_while_the_engine_holds_the_lock() {
    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let w = checkout(&base, &url, "w", &[]);
    std::fs::write(w.join("f.txt"), "x\n").expect("write");
    svn(&w, &["add", "-q", "f.txt"]);

    let db = WcDb::open(&w).expect("open");
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("lock");

    let out = svn_command(&w)
        .args(["commit", "-m", "x"])
        .output()
        .expect("svn");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("E155004") || stderr.contains("locked"),
        "{stderr}"
    );
    // Read-only commands are not blocked by a WC lock.
    svn(&w, &["status"]);

    db.wc_lock_release(&RelPath::root()).expect("release");
    svn(&w, &["commit", "-q", "-m", "x"]);
}

/// svn holds the lock for the whole commit; a pre-commit hook keeps it running for a while.
#[cfg(unix)]
#[test]
fn engine_detects_and_respects_a_lock_held_by_a_running_svn() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let hook = base.join("repo/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 4\nexit 0\n").expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let w = checkout(&base, &url, "w", &[]);
    std::fs::write(w.join("f.txt"), "x\n").expect("write");
    svn(&w, &["add", "-q", "f.txt"]);

    let mut child = svn_command(&w)
        .args(["commit", "-q", "-m", "slow"])
        .spawn()
        .expect("spawn svn");
    let db = WcDb::open(&w).expect("open");
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, std::process::id()));

    let deadline = Instant::now() + Duration::from_secs(15);
    while db.wc_lock_held(&RelPath::root()).expect("held") == LockState::Free {
        assert!(Instant::now() < deadline, "svn never took its lock");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther
    );
    let err = db
        .wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect_err("svn holds it");
    assert_eq!(err.code, ErrorCode::WcBusy);
    assert!(
        db.wc_cleanup_locks().expect("cleanup").recovered.is_empty(),
        "svn's lock must survive cleanup"
    );
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther
    );

    assert!(
        child.wait().expect("wait").success(),
        "svn commit must finish despite our probing"
    );
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::Free
    );
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("free after svn finished");
}

/// The race the marker closes: app crash, then `svn cleanup`, then a new svn operation locks the
/// same directory; our recovery must leave that lock alone.
#[cfg(unix)]
#[test]
fn svn_cleanup_after_a_crash_then_a_new_svn_lock_is_never_taken_over() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let hook = base.join("repo/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 4\nexit 0\n").expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let w = checkout(&base, &url, "w", &[]);
    std::fs::write(w.join("f.txt"), "x\n").expect("write");
    svn(&w, &["add", "-q", "f.txt"]);

    let dead = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_dead = dead.clone();
    let store = Arc::new(MemoryLockOwnership::new(Arc::new(
        move |pid: u32, _: &str| {
            !(pid == 111 && probe_dead.load(std::sync::atomic::Ordering::SeqCst))
        },
    )));
    {
        let crashing = WcDb::open(&w).expect("open");
        crashing.set_lock_owner(owner(&store, 111));
        crashing
            .wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
            .expect("lock");
    }
    dead.store(true, std::sync::atomic::Ordering::SeqCst);
    svn(&w, &["cleanup"]); // clears the row and (we rely on it) .svn/tmp

    let mut child = svn_command(&w)
        .args(["commit", "-q", "-m", "slow"])
        .spawn()
        .expect("spawn svn");
    let db = WcDb::open(&w).expect("open");
    db.set_lock_owner(owner(&store, 222));
    let deadline = Instant::now() + Duration::from_secs(15);
    while db.wc_lock_held(&RelPath::root()).expect("held") == LockState::Free {
        assert!(Instant::now() < deadline, "svn never took its lock");
        std::thread::sleep(Duration::from_millis(50));
    }
    let report = db.wc_cleanup_locks().expect("cleanup");
    assert!(report.recovered.is_empty(), "{report:?}");
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther
    );
    assert!(child.wait().expect("wait").success());
}

#[test]
fn stale_tmp_files_of_the_engine_go_and_nothing_else() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = wc(tmp.path());
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    let t = tmp.path().join(".svn/tmp");
    let ours = [
        "wq-gzeqsJ",
        "update-a1",
        "merge-b2",
        "pristine-c3",
        "marker-d4",
        "external-e5",
        "wc.db.creating",
        "wc.db.creating-journal",
        "wc.db.creating.lock",
    ];
    let kept = ["svn-AbCd", "notes.txt", "wq-", "update", "random", "wc.db"];
    for n in ours.iter().chain(kept.iter()) {
        std::fs::write(t.join(n), b"x").expect("write");
    }
    std::fs::create_dir(t.join("wq-dir")).expect("dir");
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("victim"), b"v").expect("victim");
        std::os::unix::fs::symlink(outside.path().join("victim"), t.join("wq-link")).expect("link");
        std::mem::forget(outside);
    }

    // Without the whole-tree write lock: refused, nothing removed.
    assert!(db.remove_stale_tmp_files().is_err());
    db.wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
        .expect("subtree lock");
    assert!(db.remove_stale_tmp_files().is_err());
    db.wc_lock_release(&rp("a")).expect("release");
    for n in ours {
        assert!(t.join(n).exists(), "{n}");
    }

    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("root lock");
    // A pending work item may still name a temp file: nothing goes.
    db.queue_work(&[Skel::parse(b"(file-remove 18 .svn/tmp/wq-gzeqsJ)").expect("skel")])
        .expect("queue");
    assert_eq!(db.remove_stale_tmp_files().expect("pending"), 0);
    assert!(t.join("wq-gzeqsJ").exists());
    let (id, _) = db.work_items().expect("items")[0].clone();
    db.complete_work_item(id).expect("complete");

    assert_eq!(db.remove_stale_tmp_files().expect("remove"), ours.len());
    for n in ours {
        assert!(!t.join(n).exists(), "{n} left");
    }
    for n in kept {
        assert!(t.join(n).exists(), "{n} removed");
    }
    assert!(t.join("wq-dir").is_dir());
    #[cfg(unix)]
    assert!(t.join("wq-link").symlink_metadata().is_ok());
    // The lock's own marker survives.
    assert!(std::fs::read_dir(&t).expect("read").any(|e| {
        e.expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with("opensvn-lock-")
    }));
    db.wc_lock_release(&RelPath::root()).expect("release");
}

/// `svn cleanup` breaks our lock (`break_locks`) and empties `.svn/tmp`, our marker included.
/// A write that verifies the lock fails then, the lock is forgotten, and releasing it later
/// leaves the row of the client that took the working copy over alone.
#[test]
fn a_lock_broken_by_another_client_is_detected_and_its_new_row_left_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = wc(tmp.path());
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, 1));
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("lock");
    db.with_txn(|t| t.verify_wc_lock()).expect("still ours");

    // svn cleanup: row deleted and taken again by svn, `.svn/tmp` emptied.
    let tmp_dir = tmp.path().join(".svn/tmp");
    for e in std::fs::read_dir(&tmp_dir).expect("tmp").flatten() {
        std::fs::remove_file(e.path()).expect("rm");
    }
    db.with_write(|tx| {
        tx.execute("DELETE FROM wc_lock", []).map_err(sql_err)?;
        tx.execute(
            "INSERT INTO wc_lock (wc_id, local_dir_relpath, locked_levels) VALUES (1, '', -1)",
            [],
        )
        .map_err(sql_err)?;
        Ok(())
    })
    .expect("svn takes it");

    let err = db
        .with_txn(|t| {
            t.verify_wc_lock()?;
            t.base_add(&rp("d"), &base_dir("d"))
        })
        .expect_err("lock broken");
    assert_eq!(err.code, ErrorCode::WcBusy, "{err}");
    assert!(
        err.message
            .contains("cleaned up by another client during the update"),
        "{err}"
    );
    assert!(
        db.read_base(&rp("d")).expect("read").is_none(),
        "nothing written"
    );
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther,
        "no longer ours"
    );
    db.wc_lock_release(&RelPath::root())
        .expect("release is quiet");
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther,
        "svn's row stays"
    );
    assert!(
        store
            .owned_by_dead_process(&tmp.path().canonicalize().expect("c"))
            .expect("records")
            .is_empty()
    );

    // svn releases; we can lock again.
    db.with_write(|tx| {
        tx.execute("DELETE FROM wc_lock", []).map_err(sql_err)?;
        Ok(())
    })
    .expect("svn done");
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("lock again");
    db.with_txn(|t| t.verify_wc_lock()).expect("ours again");

    // Only the row gone (marker kept) is a broken lock too.
    db.with_write(|tx| {
        tx.execute("DELETE FROM wc_lock", []).map_err(sql_err)?;
        Ok(())
    })
    .expect("break");
    let err = db.with_txn(|t| t.verify_wc_lock()).expect_err("row gone");
    assert_eq!(err.code, ErrorCode::WcBusy);
    db.wc_lock_release(&RelPath::root()).expect("release");
    assert!(
        std::fs::read_dir(&tmp_dir)
            .expect("tmp")
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with("opensvn-lock-")),
        "marker of the lost lock removed"
    );

    // A release that finds its marker gone (no verify in between) leaves the row alone too.
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Normal)
        .expect("lock");
    for e in std::fs::read_dir(&tmp_dir).expect("tmp").flatten() {
        std::fs::remove_file(e.path()).expect("rm");
    }
    db.wc_lock_release(&RelPath::root()).expect("release");
    assert_eq!(
        db.wc_lock_held(&RelPath::root()).expect("held"),
        LockState::HeldByOther,
        "the row is not ours to delete any more"
    );
}

/// The race against the real `svn cleanup`: we hold the lock and have reused an unreferenced
/// pristine row ("already stored", the update's `place`), the NODES transaction is still to
/// come. svn breaks our lock, vacuums the row with its file and empties `.svn/tmp` (our kept
/// copy too). The transaction must fail without re-creating the row, and releasing must leave
/// the working copy usable by svn.
#[test]
fn real_svn_cleanup_between_place_and_the_nodes_transaction() {
    use svn_wc::db::BaseNode;
    use svn_wc::pristine::{Placement, PristineIndex as _};

    if skip_without_svn() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().canonicalize().expect("canon");
    let url = create_repo(&base);
    let w = checkout(&base, &url, "w", &[]);

    let db = WcDb::open(&w).expect("open");
    let store = Arc::new(MemoryLockOwnership::for_current_process("boot"));
    db.set_lock_owner(owner(&store, std::process::id()));
    let text = b"left behind by an earlier operation\n";
    let rec = db
        .pristine_store()
        .install(&mut &text[..], None, None)
        .expect("install");
    db.wc_lock_obtain(&RelPath::root(), -1, ObtainMode::Cleanup)
        .expect("lock");
    let copy = {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new_in(w.join(".svn/tmp")).expect("temp");
        f.write_all(text).expect("write");
        f.into_temp_path()
    };
    let Placement::AlreadyStored(spare) = db
        .pristine_store()
        .place(copy, &rec, true, &mut std::collections::HashSet::new())
        .expect("place")
    else {
        panic!("the unreferenced row is reused");
    };

    svn(&w, &["cleanup"]);

    let stored = db.pristine_store().path_of(&rec.sha1);
    assert!(!stored.exists(), "svn vacuumed the unreferenced text");
    assert!(!spare.exists(), "svn emptied .svn/tmp");
    let node = BaseNode {
        kind: NodeKind::File,
        checksum: Some(rec.sha1.to_svn_string()),
        properties: Some(Vec::new()),
        depth: None,
        ..base_dir("g.txt")
    };
    let err = db
        .with_txn(|t| {
            t.verify_wc_lock()?;
            t.record_pristine_or_restore(&rec, Some(&spare))?;
            t.base_add(&rp("g.txt"), &node)
        })
        .expect_err("cleaned up");
    assert_eq!(err.code, ErrorCode::WcBusy, "{err}");
    assert!(
        err.message
            .contains("cleaned up by another client during the update"),
        "{err}"
    );
    // Without the lock check the pristine check still refuses the row.
    let err = db
        .with_txn(|t| {
            t.record_pristine_or_restore(&rec, Some(&spare))?;
            t.base_add(&rp("g.txt"), &node)
        })
        .expect_err("text gone");
    assert_eq!(err.code, ErrorCode::WcBusy, "{err}");
    assert!(
        db.pristine_store()
            .index()
            .lookup(&rec.sha1)
            .expect("lookup")
            .is_none(),
        "no row without its file"
    );
    assert!(db.read_base(&rp("g.txt")).expect("read").is_none());

    db.wc_lock_release(&RelPath::root()).expect("release");
    drop(spare);
    drop(db);
    // svn sees a consistent, unlocked working copy.
    assert_eq!(svn_stdout(&w, &["status"]).trim(), "");
    svn(&w, &["cleanup"]);
    svn(&w, &["update", "-q"]);
}
