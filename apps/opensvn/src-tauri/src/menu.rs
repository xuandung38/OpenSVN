//! Native menu bar (doc 01 §8). Items of our own emit `menu://<id>` to the WebView, which runs the
//! same action as the matching keyboard shortcut; the standard Edit items are native.

use tauri::Emitter;
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};

/// App menu items: id (the event is `menu://<id>`), label and accelerator.
pub const FILE_ITEMS: [(&str, &str, &str); 3] = [
    ("checkout", "Checkout Repository…", "CmdOrCtrl+Shift+O"),
    ("add-working-copy", "Add Existing Working Copy…", "CmdOrCtrl+O"),
    ("switch-repository", "Switch Repository…", "CmdOrCtrl+T"),
];

/// Event name for a menu item id of ours; `None` for native items (copy, paste, quit, ...).
pub fn event_for(id: &str) -> Option<String> {
    let ours = FILE_ITEMS.iter().any(|(i, _, _)| *i == id) || id == "settings";
    ours.then(|| format!("menu://{id}"))
}

/// Builds the menu bar for `app`.
pub fn build<R: tauri::Runtime>(app: &tauri::App<R>) -> tauri::Result<Menu<R>> {
    let mut file = SubmenuBuilder::new(app, "File");
    for (id, label, accel) in FILE_ITEMS {
        file = file.item(&MenuItemBuilder::with_id(id, label).accelerator(accel).build(app)?);
    }
    let settings = MenuItemBuilder::with_id("settings", "Settings…")
        .accelerator("CmdOrCtrl+,")
        .build(app)?;
    if !cfg!(target_os = "macos") {
        file = file.separator().item(&settings).separator().quit();
    }
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let file = file.build()?;
    let mut bar = MenuBuilder::new(app);
    if cfg!(target_os = "macos") {
        let app_menu = SubmenuBuilder::new(app, "OpenSVN")
            .about(None)
            .separator()
            .item(&settings)
            .separator()
            .hide()
            .hide_others()
            .show_all()
            .separator()
            .quit()
            .build()?;
        bar = bar.item(&app_menu);
    }
    bar.item(&file).item(&edit).build()
}

/// Installs the menu bar and forwards our items to the WebView as `menu://<id>`.
pub fn install<R: tauri::Runtime>(app: &tauri::App<R>) -> tauri::Result<()> {
    app.set_menu(build(app)?)?;
    app.on_menu_event(|handle, event| {
        if let Some(name) = event_for(event.id().as_ref())
            && let Err(e) = handle.emit(&name, ())
        {
            tracing::warn!(error = %e, "menu event not delivered");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_items_become_events() {
        assert_eq!(event_for("checkout").as_deref(), Some("menu://checkout"));
        assert_eq!(
            event_for("switch-repository").as_deref(),
            Some("menu://switch-repository")
        );
        assert_eq!(event_for("settings").as_deref(), Some("menu://settings"));
        assert_eq!(event_for("quit"), None);
        assert_eq!(event_for("../../x"), None);
    }
}
