#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;

use super::*;

/// Whether Tauri accepts `name` as an event name (`tauri::event::event_name`, which it does not
/// export).
fn is_valid_event_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '/' | ':' | '_'))
}

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
        "file.switch-repo",
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
    assert_eq!(accelerator("Mod+,"), "CmdOrCtrl+,");
    assert_eq!(accelerator("Ctrl+Q"), "Ctrl+Q");
    assert_eq!(event_name("wc.update"), "menu://wc/update");
    assert_eq!(event_name("app.settings"), "menu://app/settings");
    assert!(is_valid_event_name("menu://view/diff-mode"));
    for bad in ["menu://wc.update", "", "a b", "x\r"] {
        assert!(!is_valid_event_name(bad), "{bad:?}");
    }
}

/// Tauri refuses an event name outside `[alnum - / : _]` on `emit` (Rust) and on `listen` (the
/// UI's subscription fails): a menu item whose event breaks that rule silently does nothing.
#[test]
fn every_menu_event_is_a_name_tauri_accepts() {
    for os in [Os::Darwin, Os::Windows, Os::Linux] {
        for layout in [true, false] {
            let spec = menu_spec(os, layout);
            let mut all = Vec::new();
            flatten(&spec, &mut all);
            for e in all {
                if let Entry::Item { id, .. } = e
                    && let Some(name) = event_for(id)
                {
                    assert!(is_valid_event_name(&name), "{name} ({os:?})");
                }
            }
        }
    }
    let ui = ui_menu_events();
    assert!(ui.contains("menu://app/settings"), "{ui:?}");
    for name in ui {
        assert!(is_valid_event_name(&name), "{name} in MENU_EVENTS");
    }
}

/// The same check through Tauri itself: emitting a menu event succeeds, a dotted name fails.
#[test]
fn tauri_emits_the_menu_events() {
    use tauri::Emitter;
    let app = tauri::test::mock_app();
    for id in [
        "app.settings",
        "wc.update",
        "view.diff-mode",
        "window.conflicts",
    ] {
        let name = event_for(id).unwrap();
        app.emit(&name, ())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    assert!(app.emit("menu://app.settings", ()).is_err());
}

/// Settings opens with Cmd+, on macOS (app menu) and Ctrl+, elsewhere (File menu).
#[test]
fn settings_is_mod_comma_on_every_os() {
    for os in [Os::Darwin, Os::Windows, Os::Linux] {
        let spec = menu_spec(os, true);
        assert_eq!(accel_of(&spec, "app.settings"), Some("Mod+,"), "{os:?}");
    }
}

#[test]
fn only_items_of_the_spec_become_events() {
    assert_eq!(
        event_for("file.checkout").as_deref(),
        Some("menu://file/checkout")
    );
    assert_eq!(
        event_for("file.switch-repo").as_deref(),
        Some("menu://file/switch-repo")
    );
    // the Branch-only item and the macOS-only item are known in every state
    assert!(event_for("branch.merge").is_some());
    assert!(event_for("window.conflicts").is_some());
    assert_eq!(
        event_for("app.settings").as_deref(),
        Some("menu://app/settings")
    );
    for id in ["quit", "copy", "", "../../x", "menu://wc/update"] {
        assert_eq!(event_for(id), None, "{id}");
    }
}

// macOS creates native menu items on the main thread only, which test threads are not.
#[cfg(not(target_os = "macos"))]
#[test]
fn the_native_menu_builds_for_every_os_description() {
    let app = tauri::test::mock_app();
    for os in [Os::Darwin, Os::Windows, Os::Linux] {
        for layout in [true, false] {
            build_menu(&app, os, layout).unwrap();
        }
    }
}

/// Event names the UI subscribes to (`MENU_EVENTS` in `src/ipc/events.ts`).
fn ui_menu_events() -> HashSet<String> {
    let ts = include_str!("../../../src/ipc/events.ts");
    ts.split("\"menu://")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(|rest| format!("menu://{rest}"))
        .collect()
}

/// Menu item ids behind [`ui_menu_events`] (`menu://wc/update` -> `wc.update`).
fn ui_menu_ids() -> HashSet<String> {
    ui_menu_events()
        .into_iter()
        .map(|name| name["menu://".len()..].replace('/', "."))
        .collect()
}

#[test]
fn every_menu_event_the_ui_subscribes_to_is_an_item_of_this_menu() {
    let ids = ui_menu_ids();
    assert!(ids.len() >= 10, "{ids:?}");
    for id in &ids {
        assert!(
            event_for(id).is_some(),
            "the UI listens to unknown or natively handled menu id {id}"
        );
    }
}

/// The other direction: no item (with or without an accelerator) does nothing when activated.
/// Each one is either subscribed by the UI or handled natively; predefined items are native roles.
#[test]
fn every_item_of_this_menu_is_handled_by_the_ui_or_natively() {
    let ui = ui_menu_ids();
    for os in [Os::Darwin, Os::Windows, Os::Linux] {
        for layout in [true, false] {
            let spec = menu_spec(os, layout);
            let mut all = Vec::new();
            flatten(&spec, &mut all);
            for e in all {
                let Entry::Item { id, accel, .. } = e else {
                    continue;
                };
                let native = native_action(id).is_some();
                let in_ui = ui.contains(*id);
                assert!(
                    native ^ in_ui,
                    "{id} (accelerator {accel:?}, {os:?}) must be handled by exactly one of the UI ({in_ui}) or Rust ({native})"
                );
                assert_eq!(event_for(id).is_some(), in_ui, "{id}");
            }
        }
    }
}

#[test]
fn zoom_steps_like_a_browser_and_stays_in_range() {
    assert_eq!(next_zoom(100, NativeAction::ZoomIn), 110);
    assert_eq!(next_zoom(100, NativeAction::ZoomOut), 90);
    assert_eq!(next_zoom(300, NativeAction::ZoomIn), 300);
    assert_eq!(next_zoom(50, NativeAction::ZoomOut), 50);
    assert_eq!(next_zoom(175, NativeAction::ZoomReset), 100);
    assert_eq!(next_zoom(105, NativeAction::ZoomIn), 110);
    assert_eq!(next_zoom(105, NativeAction::ZoomOut), 100);
    for id in [
        "view.zoom-in",
        "view.zoom-out",
        "view.zoom-reset",
        "app.exit",
    ] {
        assert!(native_action(id).is_some(), "{id}");
        assert_eq!(event_for(id), None, "{id} is not forwarded to the UI");
    }
}

/// `SHORTCUTS` in `src/features/wc/shortcuts.ts`: id -> (key, shift, alt).
fn ui_shortcuts() -> std::collections::HashMap<String, (String, bool, bool)> {
    let ts = include_str!("../../../src/features/wc/shortcuts.ts");
    let block = ts
        .split("export const SHORTCUTS")
        .nth(1)
        .and_then(|rest| rest.split("\n};").next())
        .unwrap();
    block
        .lines()
        .filter_map(|line| {
            let (id, spec) = line.trim().split_once(": { key: \"")?;
            let (key, flags) = spec.split_once('"')?;
            Some((
                id.to_owned(),
                (
                    key.replace("\\\\", "\\"),
                    flags.contains("shift: true"),
                    flags.contains("alt: true"),
                ),
            ))
        })
        .collect()
}

/// `MENU_ACTIONS` in `src/features/wc/use-wc-shortcuts.ts`: menu item id -> action.
fn ui_menu_actions() -> Vec<(String, String)> {
    let ts = include_str!("../../../src/features/wc/use-wc-shortcuts.ts");
    ts.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("\"menu://")?;
            let (id, action) = rest.split_once("\": \"")?;
            Some((
                id.replace('/', "."),
                action.trim_end_matches("\",").to_owned(),
            ))
        })
        .collect()
}

fn accel_of(spec: &[Entry], wanted: &str) -> Option<&'static str> {
    let mut all = Vec::new();
    flatten(spec, &mut all);
    all.iter().find_map(|e| match e {
        Entry::Item { id, accel, .. } if *id == wanted => *accel,
        _ => None,
    })
}

/// The echo suppression needs the native accelerator and the UI's keydown shortcut of one action
/// to be the same keys, so a key press is recognized from both sides.
#[test]
fn ui_shortcuts_use_the_same_keys_as_the_menu_accelerators() {
    let shortcuts = ui_shortcuts();
    assert!(shortcuts.len() >= 20, "{shortcuts:?}");
    let actions = ui_menu_actions();
    assert_eq!(actions.len(), ui_menu_ids().len(), "{actions:?}");
    let spec = menu_spec(Os::Linux, true);
    let mut checked = 0;
    for (id, action) in &actions {
        let Some((key, shift, alt)) = shortcuts.get(action) else {
            continue;
        };
        let Some(accel) = accel_of(&spec, id) else {
            continue;
        };
        let parts: Vec<&str> = accel.split('+').collect();
        let (accel_key, mods) = parts.split_last().unwrap();
        assert!(
            accel_key.eq_ignore_ascii_case(key),
            "{id}: {accel} vs {key}"
        );
        assert!(mods.contains(&"Mod"), "{id}: {accel}");
        assert_eq!(mods.contains(&"Shift"), *shift, "{id}: {accel}");
        assert_eq!(mods.contains(&"Alt"), *alt, "{id}: {accel}");
        assert_eq!(
            mods.len(),
            1 + usize::from(*shift) + usize::from(*alt),
            "{id}"
        );
        checked += 1;
    }
    assert!(checked >= 25, "only {checked} accelerators compared");
}

/// Items the UI still answers with "not available yet" (planned for later phases). Every other
/// item must do something; implementing one of these removes it from the list.
#[test]
fn only_planned_items_are_not_yet_available() {
    let not_yet: Vec<String> = ui_menu_actions()
        .into_iter()
        .filter(|(_, action)| action == "notYet")
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        not_yet,
        ["app.check-app-updates", "wc.upgrade", "branch.merge",]
    );
}
