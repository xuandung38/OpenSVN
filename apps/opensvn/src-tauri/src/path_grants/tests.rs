#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use opensvn_ipc::{ErrorCode, GrantKind};

use super::*;

fn dir() -> PathBuf {
    std::env::temp_dir().join("project")
}

#[test]
fn grant_is_single_use() {
    let g = PathGrants::new();
    let info = g.mint(dir(), GrantKind::Dir, MAIN_WINDOW);
    assert_eq!(info.display_name, "project");
    assert_eq!(
        g.redeem(&info.id, MAIN_WINDOW, GrantKind::Dir).unwrap(),
        dir()
    );
    let err = g.redeem(&info.id, MAIN_WINDOW, GrantKind::Dir).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidArgument);
}

#[test]
fn grant_expires() {
    let g = PathGrants::with_ttl(Duration::from_millis(20));
    let info = g.mint(dir(), GrantKind::Dir, MAIN_WINDOW);
    std::thread::sleep(Duration::from_millis(40));
    assert!(g.redeem(&info.id, MAIN_WINDOW, GrantKind::Dir).is_err());
}

#[test]
fn grant_is_bound_to_its_window_and_kind_and_consumed_by_a_wrong_try() {
    let g = PathGrants::new();
    let info = g.mint(dir(), GrantKind::Dir, MAIN_WINDOW);
    assert!(g.redeem(&info.id, "other", GrantKind::Dir).is_err());
    assert!(
        g.redeem(&info.id, MAIN_WINDOW, GrantKind::Dir).is_err(),
        "consumed by the failed try"
    );
    let info = g.mint(dir(), GrantKind::File, MAIN_WINDOW);
    assert!(g.redeem(&info.id, MAIN_WINDOW, GrantKind::Dir).is_err());
    assert_eq!(g.outstanding(), 0);
}

#[test]
fn unknown_ids_and_expired_entries_are_cleaned() {
    let g = PathGrants::with_ttl(Duration::from_millis(10));
    assert!(
        g.redeem(&PathGrantId("nope".into()), MAIN_WINDOW, GrantKind::Dir)
            .is_err()
    );
    g.mint(dir(), GrantKind::Dir, MAIN_WINDOW);
    std::thread::sleep(Duration::from_millis(30));
    g.mint(dir(), GrantKind::Dir, MAIN_WINDOW);
    assert_eq!(
        g.outstanding(),
        1,
        "the expired grant was dropped when minting"
    );
}
