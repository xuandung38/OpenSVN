#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use super::*;

fn labels(spec: &[Entry]) -> Vec<&'static str> {
    spec.iter()
        .filter_map(|e| {
            if let Entry::Submenu { label, .. } = e {
                Some(*label)
            } else {
                None
            }
        })
        .collect()
}

fn flatten<'a>(entries: &'a [Entry], out: &mut Vec<&'a Entry>) {
    for e in entries {
        out.push(e);
        if let Entry::Submenu { entries, .. } = e {
            flatten(entries, out);
        }
    }
}

fn submenu<'a>(spec: &'a [Entry], name: &str) -> &'a [Entry] {
    spec.iter()
        .find_map(|e| match e {
            Entry::Submenu { label, entries } if *label == name => Some(entries.as_slice()),
            _ => None,
        })
        .unwrap()
}

fn has_item(entries: &[Entry], wanted: &str) -> bool {
    let mut all = Vec::new();
    flatten(entries, &mut all);
    all.iter()
        .any(|e| matches!(e, Entry::Item { id, .. } if *id == wanted))
}

#[test]
fn macos_has_an_app_menu_and_a_window_menu() {
    let spec = menu_spec(Os::Darwin, true);
    assert_eq!(
        labels(&spec),
        [
            "OpenSVN",
            "File",
            "Edit",
            "View",
            "Working Copy",
            "Branch",
            "Selection",
            "Window",
            "Help"
        ]
    );
    let app = submenu(&spec, "OpenSVN");
    assert!(has_item(app, "app.settings") && has_item(app, "app.check-app-updates"));
    assert!(matches!(app.last(), Some(Entry::Predefined(Predef::Quit))));
    // settings and exit live in the app menu, not in File
    assert!(!has_item(submenu(&spec, "File"), "app.settings"));
    assert!(!has_item(submenu(&spec, "File"), "app.exit"));
    assert!(has_item(submenu(&spec, "Window"), "window.conflicts"));
}

#[test]
fn windows_and_linux_put_settings_exit_and_about_in_file_and_help() {
    for os in [Os::Windows, Os::Linux] {
        let spec = menu_spec(os, true);
        assert_eq!(
            labels(&spec),
            [
                "File",
                "Edit",
                "View",
                "Working Copy",
                "Branch",
                "Selection",
                "Help"
            ]
        );
        let file = submenu(&spec, "File");
        assert!(has_item(file, "app.settings") && has_item(file, "app.exit"));
        let help = submenu(&spec, "Help");
        assert!(has_item(help, "app.check-app-updates"));
        assert!(
            help.iter()
                .any(|e| matches!(e, Entry::Predefined(Predef::About)))
        );
    }
}

#[test]
fn working_copy_and_file_menus_contain_the_documented_items() {
    let spec = menu_spec(Os::Linux, true);
    for id in [
        "file.checkout",
        "file.add-wc",
        "file.recent",
        "file.close-wc",
    ] {
        assert!(has_item(submenu(&spec, "File"), id), "{id}");
    }
    for id in [
        "wc.check-updates",
        "wc.update",
        "wc.update-to",
        "wc.commit",
        "wc.discard-all",
        "wc.sparse",
        "wc.cleanup",
        "wc.properties",
        "wc.layout",
        "wc.upgrade",
        "wc.reveal",
        "wc.terminal",
        "wc.editor",
    ] {
        assert!(has_item(submenu(&spec, "Working Copy"), id), "{id}");
    }
    for id in [
        "view.changes",
        "view.history",
        "view.browse",
        "view.activity",
        "view.sidebar",
        "view.zoom-in",
    ] {
        assert!(has_item(submenu(&spec, "View"), id), "{id}");
    }
    for id in [
        "sel.lock",
        "sel.unlock",
        "sel.log",
        "sel.blame",
        "sel.rename",
        "sel.delete",
    ] {
        assert!(has_item(submenu(&spec, "Selection"), id), "{id}");
    }
}

#[test]
fn branch_menu_becomes_location_without_a_layout() {
    let with = menu_spec(Os::Linux, true);
    let without = menu_spec(Os::Linux, false);
    assert!(labels(&with).contains(&"Branch") && !labels(&with).contains(&"Location"));
    assert!(labels(&without).contains(&"Location"));
    assert!(has_item(submenu(&with, "Branch"), "branch.merge"));
    assert!(!has_item(submenu(&without, "Location"), "branch.merge"));
    for id in [
        "branch.switch",
        "branch.new",
        "branch.compare",
        "branch.show-on-server",
        "branch.layout",
    ] {
        assert!(has_item(submenu(&without, "Location"), id), "{id}");
    }
}

#[test]
fn ids_are_unique_and_accelerators_do_not_collide_per_platform() {
    for os in [Os::Darwin, Os::Windows, Os::Linux] {
        let spec = menu_spec(os, true);
        let mut all = Vec::new();
        flatten(&spec, &mut all);
        let mut ids = HashSet::new();
        let mut accels = HashSet::new();
        for e in all {
            if let Entry::Item { id, accel, .. } = e {
                assert!(ids.insert(*id), "duplicate id {id} on {os:?}");
                if let Some(a) = accel {
                    assert!(
                        accels.insert(accelerator(a)),
                        "duplicate accelerator {a} on {os:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn accelerators_and_event_names_follow_the_tauri_conventions() {
    assert_eq!(accelerator("Mod+Shift+K"), "CmdOrCtrl+Shift+K");
    assert_eq!(accelerator("Ctrl+,"), "Ctrl+,");
    assert_eq!(event_name("wc.update"), "menu://wc.update");
}

#[test]
fn the_native_menu_builds_for_every_os_description() {
    let app = tauri::test::mock_app();
    for os in [Os::Darwin, Os::Windows, Os::Linux] {
        for layout in [true, false] {
            build_menu(&app, os, layout).unwrap();
        }
    }
}
