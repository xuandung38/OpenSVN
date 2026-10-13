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
        db.wc_cleanup_locks().expect("cleanup").is_empty(),
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
        db.wc_cleanup_locks().expect("cleanup").is_empty(),
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
    assert_eq!(db.wc_cleanup_locks().expect("cleanup"), vec![rp("a")]);
    assert_eq!(db.wc_lock_held(&rp("a")).expect("held"), LockState::Free);
    db.wc_lock_obtain(&rp("a"), -1, ObtainMode::Normal)
        .expect("lock after recovery");
}

#[test]
fn orphan_lock_rows_for_unversioned_directories_are_dropped() {
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
    assert_eq!(db.wc_cleanup_locks().expect("cleanup"), vec![rp("gone")]);
    assert_eq!(
        db.wc_lock_held(&rp("c")).expect("held"),
        LockState::HeldByOther,
        "versioned dir keeps its row"
    );
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
        db.wc_cleanup_locks().expect("cleanup").is_empty(),
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
