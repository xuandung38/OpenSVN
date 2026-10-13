use std::path::Path;
use std::sync::Arc;

use svn_types::{ErrorCode, LockOwnershipStore, RelPath};

use super::*;

fn store(alive: impl Fn(u32, &str) -> bool + Send + Sync + 'static) -> SqliteLockOwnership {
    SqliteLockOwnership::new(AppDb::open_in_memory().unwrap(), Arc::new(alive))
}

fn dir(s: &str) -> RelPath {
    RelPath::new(s).unwrap()
}

#[test]
fn live_owner_blocks_another_process_and_dead_owner_is_replaced() {
    // pid 7 is alive, everything else is dead
    let s = store(|pid, _| pid == 7);
    let root = Path::new("/wc");
    s.record(root, &dir("a"), 7, "b").unwrap();
    assert_eq!(
        s.record(root, &dir("a"), 8, "b").unwrap_err().code,
        ErrorCode::WcBusy
    );
    // same owner may re-record (re-entrant)
    s.record(root, &dir("a"), 7, "b").unwrap();
    s.forget(root, &dir("a")).unwrap();
    s.record(root, &dir("a"), 8, "b").unwrap();
    // 8 is dead for this probe: 9 takes over
    s.record(root, &dir("a"), 9, "b").unwrap();
}

#[test]
fn reports_directories_of_dead_owners_sorted() {
    let s = store(|pid, _| pid == 7);
    let root = Path::new("/wc");
    s.record(root, &dir("z"), 8, "b").unwrap();
    s.record(root, &dir("a/b"), 8, "b").unwrap();
    s.record(root, &dir("live"), 7, "b").unwrap();
    s.record(Path::new("/other"), &dir("x"), 8, "b").unwrap();
    assert_eq!(
        s.owned_by_dead_process(root).unwrap(),
        vec![dir("a/b"), dir("z")]
    );
}

#[test]
fn different_boot_id_counts_as_dead_through_the_probe() {
    let s = store(|_, boot| boot == "boot-1");
    let root = Path::new("/wc");
    s.record(root, &dir("a"), 1, "boot-0").unwrap();
    assert_eq!(s.owned_by_dead_process(root).unwrap(), vec![dir("a")]);
}

#[test]
fn wc_root_is_normalized_lexically() {
    let s = store(|_, _| false);
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

#[test]
fn records_survive_a_new_store_over_the_same_database() {
    let db = AppDb::open_in_memory().unwrap();
    let s1 = SqliteLockOwnership::new(db.clone(), Arc::new(|_: u32, _: &str| true));
    s1.record(Path::new("/wc"), &dir("a"), 7, "b").unwrap();
    let s2 = SqliteLockOwnership::new(db, Arc::new(|_: u32, _: &str| false));
    assert_eq!(
        s2.owned_by_dead_process(Path::new("/wc")).unwrap(),
        vec![dir("a")]
    );
}

#[test]
fn real_probe_works_end_to_end_for_the_current_process() {
    let s = SqliteLockOwnership::with_os_probe(AppDb::open_in_memory().unwrap());
    let root = Path::new("/wc");
    s.record(root, &dir("a"), std::process::id(), current_boot_id())
        .unwrap();
    assert!(s.owned_by_dead_process(root).unwrap().is_empty());
    s.record(root, &dir("old"), u32::MAX - 3, current_boot_id())
        .unwrap();
    assert_eq!(s.owned_by_dead_process(root).unwrap(), vec![dir("old")]);
}
