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

#[test]
fn display_names_show_bidi_overrides_as_escapes() {
    let g = PathGrants::new();
    let info = g.mint(
        PathBuf::from("/tmp/photo\u{202E}gpj.exe"),
        GrantKind::File,
        MAIN_WINDOW,
    );
    assert_eq!(info.display_name, "photo\\u{202E}gpj.exe");
    let plain = g.mint(
        PathBuf::from("/tmp/Báo cáo.txt"),
        GrantKind::File,
        MAIN_WINDOW,
    );
    assert_eq!(plain.display_name, "Báo cáo.txt");
}

#[test]
fn dialog_text_from_the_webview_is_checked_before_a_dialog_opens() {
    use opensvn_ipc::PickRequest;
    let ok = [
        PickRequest::Folder { title: None },
        PickRequest::File {
            title: Some("Choose a file to upload".into()),
        },
        PickRequest::SaveFile {
            suggested_name: "Báo cáo.txt".into(),
            title: Some("Save".into()),
        },
    ];
    for r in &ok {
        assert!(validate_pick_request(r).is_ok(), "{r:?}");
    }
    let bad = [
        PickRequest::Folder {
            title: Some("Pick\u{202E}txt.exe".into()),
        },
        PickRequest::File {
            title: Some("line\nbreak".into()),
        },
        PickRequest::File {
            title: Some("x".repeat(500)),
        },
        PickRequest::SaveFile {
            suggested_name: "invoice\u{202E}fdp.exe".into(),
            title: None,
        },
        PickRequest::SaveFile {
            suggested_name: "../escape.txt".into(),
            title: None,
        },
        PickRequest::SaveFile {
            suggested_name: "a\\b.txt".into(),
            title: None,
        },
        PickRequest::SaveFile {
            suggested_name: "..".into(),
            title: None,
        },
        PickRequest::SaveFile {
            suggested_name: "zero\u{200B}width".into(),
            title: Some("\u{2066}".into()),
        },
    ];
    for r in &bad {
        let err = validate_pick_request(r).expect_err("must be refused");
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{r:?}");
    }
}
