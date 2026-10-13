use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use svn_types::{ErrorCode, LockOwnershipStore, RelPath};

use super::*;
use crate::db::AppDb;

/// Scripted processes: pid -> start time; boots the machine "is in".
#[derive(Default)]
struct Fake {
    procs: Mutex<HashMap<u32, u64>>,
    boot: Mutex<String>,
}

impl Fake {
    fn new(boot: &str) -> Arc<Self> {
        Arc::new(Self {
            procs: Mutex::default(),
            boot: Mutex::new(boot.to_owned()),
        })
    }
    fn spawn(&self, pid: u32, start: u64) {
        self.procs.lock().unwrap().insert(pid, start);
    }
    fn exit(&self, pid: u32) {
        self.procs.lock().unwrap().remove(&pid);
    }
    fn set_boot(&self, boot: &str) {
        *self.boot.lock().unwrap() = boot.to_owned();
    }
}

impl ProcessProbe for Fake {
    fn start_time(&self, pid: u32) -> Option<u64> {
        self.procs.lock().unwrap().get(&pid).copied()
    }
    fn same_process(&self, recorded: u64, current: u64) -> bool {
        recorded.abs_diff(current) <= 2
    }
    fn same_boot(&self, recorded: &str) -> bool {
        *self.boot.lock().unwrap() == recorded
    }
}

fn store(fake: &Arc<Fake>) -> SqliteLockOwnership {
    SqliteLockOwnership::new(AppDb::open_in_memory().unwrap(), fake.clone())
}

fn dir(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

const ROOT: &str = "/wc";

#[test]
fn live_owner_blocks_another_process_and_dead_owner_is_replaced() {
    let fake = Fake::new("b");
    fake.spawn(7, 1000);
    let s = store(&fake);
    s.record(Path::new(ROOT), &dir("a"), 7, "b").unwrap();
    assert_eq!(
        s.record(Path::new(ROOT), &dir("a"), 8, "b")
            .unwrap_err()
            .code,
        ErrorCode::WcBusy
    );
    s.record(Path::new(ROOT), &dir("a"), 7, "b").unwrap(); // same owner re-records
    fake.exit(7);
    s.record(Path::new(ROOT), &dir("a"), 9, "b").unwrap(); // dead owner replaced
}

#[test]
fn a_recycled_pid_is_dead() {
    let fake = Fake::new("b");
    fake.spawn(7, 1000);
    let s = store(&fake);
    s.record(Path::new(ROOT), &dir("a"), 7, "b").unwrap();
    // the owner died and an unrelated process got pid 7 later
    fake.exit(7);
    fake.spawn(7, 5000);
    assert_eq!(
        s.owned_by_dead_process(Path::new(ROOT)).unwrap(),
        vec![dir("a")]
    );
    s.record(Path::new(ROOT), &dir("a"), 8, "b").unwrap();
}

#[test]
fn start_time_jitter_within_tolerance_is_the_same_process() {
    let fake = Fake::new("b");
    fake.spawn(7, 1000);
    let s = store(&fake);
    s.record(Path::new(ROOT), &dir("a"), 7, "b").unwrap();
    fake.exit(7);
    fake.spawn(7, 1001);
    assert!(s.owned_by_dead_process(Path::new(ROOT)).unwrap().is_empty());
}

#[test]
fn boot_id_mismatch_with_a_matching_live_process_is_unknown_and_never_stolen() {
    let fake = Fake::new("boot-2");
    fake.spawn(7, 1000);
    let s = store(&fake);
    // recorded under a boot id that no longer matches (for example a moved boot time)
    s.record(Path::new(ROOT), &dir("a"), 7, "boot-1").unwrap();
    assert!(
        s.owned_by_dead_process(Path::new(ROOT)).unwrap().is_empty(),
        "not reported dead"
    );
    let err = s
        .record(Path::new(ROOT), &dir("a"), 8, "boot-2")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::WcBusy);
    assert!(err.message.contains("may still be running"));
}

#[test]
fn boot_change_with_no_such_process_is_dead() {
    let fake = Fake::new("boot-2");
    let s = store(&fake);
    fake.spawn(7, 1000);
    s.record(Path::new(ROOT), &dir("a"), 7, "boot-1").unwrap();
    fake.exit(7);
    assert_eq!(
        s.owned_by_dead_process(Path::new(ROOT)).unwrap(),
        vec![dir("a")]
    );
}

#[test]
fn clock_drift_alone_never_makes_a_live_owner_dead() {
    let fake = Fake::new("boot-1");
    fake.spawn(7, 1000);
    let s = store(&fake);
    s.record(Path::new(ROOT), &dir("a"), 7, "boot-1").unwrap();
    fake.set_boot("boot-1-shifted");
    assert!(s.owned_by_dead_process(Path::new(ROOT)).unwrap().is_empty());
}

#[test]
fn reports_directories_of_dead_owners_sorted_per_root() {
    let fake = Fake::new("b");
    fake.spawn(7, 1);
    let s = store(&fake);
    s.record(Path::new(ROOT), &dir("z"), 8, "b").unwrap();
    s.record(Path::new(ROOT), &dir("a/b"), 8, "b").unwrap();
    s.record(Path::new(ROOT), &dir("live"), 7, "b").unwrap();
    s.record(Path::new("/other"), &dir("x"), 8, "b").unwrap();
    assert_eq!(
        s.owned_by_dead_process(Path::new(ROOT)).unwrap(),
        vec![dir("a/b"), dir("z")]
    );
}

#[test]
fn wc_root_is_normalized_lexically_and_forgotten() {
    let fake = Fake::new("b");
    let s = store(&fake);
    s.record(Path::new("/wc/"), &dir("a"), 1, "b").unwrap();
    assert_eq!(
        s.owned_by_dead_process(Path::new("/wc/./")).unwrap(),
        vec![dir("a")]
    );
    s.forget(Path::new("/wc"), &dir("a")).unwrap();
    assert!(
        s.owned_by_dead_process(Path::new("/wc"))
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn symlinked_root_is_the_same_working_copy() {
    let dirp = tempfile::tempdir().unwrap();
    let real = dirp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dirp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let s = store(&Fake::new("b"));
    s.record(&real, &dir("a"), 1, "b").unwrap();
    assert_eq!(s.owned_by_dead_process(&link).unwrap(), vec![dir("a")]);
}

#[test]
fn records_survive_a_new_store_over_the_same_database() {
    let db = AppDb::open_in_memory().unwrap();
    let alive = Fake::new("b");
    alive.spawn(7, 1);
    SqliteLockOwnership::new(db.clone(), alive)
        .record(Path::new(ROOT), &dir("a"), 7, "b")
        .unwrap();
    let gone = Fake::new("b");
    let s2 = SqliteLockOwnership::new(db, gone);
    assert_eq!(
        s2.owned_by_dead_process(Path::new(ROOT)).unwrap(),
        vec![dir("a")]
    );
}

#[test]
fn real_probe_end_to_end_for_the_current_process() {
    let s = SqliteLockOwnership::with_os_probe(AppDb::open_in_memory().unwrap());
    let root = Path::new(ROOT);
    s.record(root, &dir("a"), std::process::id(), current_boot_id())
        .unwrap();
    assert!(s.owned_by_dead_process(root).unwrap().is_empty());
    s.record(root, &dir("old"), u32::MAX - 3, current_boot_id())
        .unwrap();
    assert_eq!(s.owned_by_dead_process(root).unwrap(), vec![dir("old")]);
}
