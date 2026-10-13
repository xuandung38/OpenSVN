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

    let out = std::process::Command::new("svn")
        .args(["commit", "-m", "x"])
        .current_dir(&w)
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

    let mut child = std::process::Command::new("svn")
        .args(["commit", "-q", "-m", "slow"])
        .current_dir(&w)
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

    let mut child = std::process::Command::new("svn")
        .args(["commit", "-q", "-m", "slow"])
        .current_dir(&w)
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
