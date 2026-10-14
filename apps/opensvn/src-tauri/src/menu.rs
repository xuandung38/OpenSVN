//! Native application menu (docs/system-design/01 section 8). This is the only menu bar.
//!
//! The menu is described as data ([`menu_spec`]) so its structure can be tested on any host, then
//! turned into a Tauri menu. Activating an item emits `menu://<id>` ([`event_name`]: dots become
//! slashes, e.g. `menu://wc/update`) for the UI, which owns the behavior and runs the same action
//! as the matching keyboard shortcut (the UI ignores the echo when one key press reaches it both
//! as a menu event and as a keydown). The few items that act on the window or the process (zoom,
//! Exit) are handled here instead ([`native_action`]), and predefined items (copy, paste, quit,
//! ...) are native roles. A test pins that every item is
//! one of these: subscribed by the UI (`MENU_EVENTS` in `src/ipc/events.ts`) or native.
//! Accelerators use `Mod` = Cmd on macOS and Ctrl elsewhere.

use tauri::Emitter;
use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem, Submenu, SubmenuBuilder};

use crate::db::Os;

/// Predefined (native behavior) items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Predef {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Hide,
    Quit,
    Minimize,
    Zoom,
    Fullscreen,
    BringAllToFront,
    About,
}

/// One entry of the menu description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A UI action: emits `menu://<id>`.
    Item {
        id: &'static str,
        label: &'static str,
        accel: Option<&'static str>,
    },
    /// A separator line.
    Separator,
    /// A native item.
    Predefined(Predef),
    /// A submenu.
    Submenu {
        label: &'static str,
        entries: Vec<Entry>,
    },
}

fn item(id: &'static str, label: &'static str, accel: Option<&'static str>) -> Entry {
    Entry::Item { id, label, accel }
}

fn sub(label: &'static str, entries: Vec<Entry>) -> Entry {
    Entry::Submenu { label, entries }
}

/// The three tabs, panels and display toggles.
fn view_menu() -> Entry {
    sub(
        "View",
        vec![
            item("view.changes", "Changes", Some("Mod+1")),
            item("view.history", "History", Some("Mod+2")),
            item("view.browse", "Browse", Some("Mod+3")),
            Entry::Separator,
            item("view.activity", "Activity panel", Some("Mod+J")),
            item("view.sidebar", "Toggle sidebar", Some("Mod+\\")),
            Entry::Separator,
            item("view.diff-mode", "Diff: split / unified", Some("Mod+Alt+D")),
            item(
                "view.hide-whitespace",
                "Hide whitespace changes",
                Some("Mod+Alt+W"),
            ),
            item("view.show-unversioned", "Show unversioned files", None),
            item("view.show-ignored", "Show ignored files", None),
            item("view.group-changelist", "Group by changelist", None),
            item("view.tree", "Show as tree", None),
            sub(
                "Theme",
                vec![
                    item("view.theme-system", "System", None),
                    item("view.theme-light", "Light", None),
                    item("view.theme-dark", "Dark", None),
                ],
            ),
            Entry::Separator,
            item("view.zoom-in", "Zoom in", Some("Mod+=")),
            item("view.zoom-out", "Zoom out", Some("Mod+-")),
            item("view.zoom-reset", "Actual size", Some("Mod+0")),
            Entry::Predefined(Predef::Fullscreen),
        ],
    )
}

fn working_copy_menu() -> Entry {
    sub(
        "Working Copy",
        vec![
            item("wc.check-updates", "Check for updates", Some("Mod+Shift+K")),
            item("wc.update", "Update", Some("Mod+Shift+U")),
            item("wc.update-to", "Update to revision…", None),
            item("wc.commit", "Commit", Some("Mod+Enter")),
            item("wc.discard-all", "Discard all changes…", None),
            Entry::Separator,
            item("wc.sparse", "Choose folders (sparse)…", None),
            item("wc.cleanup", "Cleanup…", None),
            item("wc.properties", "Properties…", None),
            item("wc.layout", "Repository layout…", None),
            item("wc.upgrade", "Upgrade working copy format…", None),
            Entry::Separator,
            item("wc.reveal", "Show in Explorer/Finder", Some("Mod+Shift+R")),
            item("wc.terminal", "Open in terminal", Some("Mod+`")),
            item("wc.editor", "Open in editor", Some("Mod+Shift+A")),
        ],
    )
}

/// `has_layout` selects the Branch or the Location variant (section 8.6).
fn branch_menu(has_layout: bool) -> Entry {
    if has_layout {
        sub(
            "Branch",
            vec![
                item("branch.switch", "Switch branch…", Some("Mod+B")),
                item("branch.new", "New branch…", Some("Mod+Shift+N")),
                item("branch.new-tag", "New tag…", Some("Mod+Shift+T")),
                item(
                    "branch.merge",
                    "Merge into current branch…",
                    Some("Mod+Shift+M"),
                ),
                item("branch.compare", "Compare with branch…", None),
                item("branch.show-on-server", "Show branch on server", None),
                item("branch.layout", "Set up branch layout…", None),
            ],
        )
    } else {
        sub(
            "Location",
            vec![
                item("branch.switch", "Switch to another folder…", Some("Mod+B")),
                item("branch.new", "Copy to new location…", Some("Mod+Shift+N")),
                item("branch.compare", "Compare with another folder…", None),
                item("branch.show-on-server", "Show on server", None),
                item("branch.layout", "Set up branch layout…", None),
            ],
        )
    }
}

fn selection_menu() -> Entry {
    sub(
        "Selection",
        vec![
            item("sel.discard", "Discard changes…", None),
            item("sel.lock", "Lock…", Some("Mod+L")),
            item("sel.unlock", "Unlock", None),
            item("sel.changelist", "Add to changelist…", None),
            item("sel.ignore", "Ignore…", None),
            item("sel.resolve", "Resolve…", None),
            Entry::Separator,
            item("sel.log", "Show log", Some("Mod+Shift+L")),
            item("sel.blame", "Blame", Some("Mod+Shift+B")),
            Entry::Separator,
            item("sel.rename", "Rename/Move…", None),
            item("sel.delete", "Delete", None),
            item("sel.properties", "Properties…", None),
        ],
    )
}

fn edit_menu() -> Entry {
    sub(
        "Edit",
        vec![
            Entry::Predefined(Predef::Undo),
            Entry::Predefined(Predef::Redo),
            Entry::Separator,
            Entry::Predefined(Predef::Cut),
            Entry::Predefined(Predef::Copy),
            Entry::Predefined(Predef::Paste),
            Entry::Predefined(Predef::SelectAll),
            Entry::Separator,
            item("edit.find", "Find / Filter", Some("Mod+F")),
            item("edit.copy-path", "Copy path", Some("Mod+Alt+C")),
            item("edit.copy-url", "Copy URL", None),
        ],
    )
}

/// The whole menu bar for `os`.
pub fn menu_spec(os: Os, has_layout: bool) -> Vec<Entry> {
    let mac = os == Os::Darwin;
    let mut file = vec![
        item("file.checkout", "Checkout repository…", Some("Mod+Shift+O")),
        item("file.add-wc", "Add existing working copy…", Some("Mod+O")),
        item("file.switch-repo", "Switch repository…", Some("Mod+T")),
        item("file.recent", "Open recent…", None),
        item("file.close-wc", "Close working copy", Some("Mod+W")),
    ];
    if !mac {
        file.push(Entry::Separator);
        file.push(item("app.settings", "Settings…", Some("Mod+,")));
        file.push(Entry::Separator);
        file.push(item("app.exit", "Exit", Some("Ctrl+Q")));
    }
    let mut help = vec![
        item("help.docs", "OpenSVN documentation", None),
        item("help.shortcuts", "Keyboard shortcuts", Some("Mod+/")),
        item("help.logs", "Show logs folder", None),
        item("help.report", "Report a problem…", None),
    ];
    if !mac {
        help.push(Entry::Separator);
        help.push(Entry::Predefined(Predef::About));
        help.push(item(
            "app.check-app-updates",
            "Check for app updates…",
            None,
        ));
    }
    let mut bar = Vec::new();
    if mac {
        bar.push(sub(
            "OpenSVN",
            vec![
                Entry::Predefined(Predef::About),
                item("app.check-app-updates", "Check for app updates…", None),
                Entry::Separator,
                item("app.settings", "Settings…", Some("Mod+,")),
                Entry::Separator,
                Entry::Predefined(Predef::Hide),
                Entry::Separator,
                Entry::Predefined(Predef::Quit),
            ],
        ));
    }
    bar.push(sub("File", file));
    bar.push(edit_menu());
    bar.push(view_menu());
    bar.push(working_copy_menu());
    bar.push(branch_menu(has_layout));
    bar.push(selection_menu());
    if mac {
        bar.push(sub(
            "Window",
            vec![
                Entry::Predefined(Predef::Minimize),
                Entry::Predefined(Predef::Zoom),
                item("window.conflicts", "Conflict resolver", None),
                Entry::Separator,
                Entry::Predefined(Predef::BringAllToFront),
            ],
        ));
    }
    bar.push(sub("Help", help));
    bar
}

/// Tauri accelerator string: `Mod` becomes `CmdOrCtrl`.
pub fn accelerator(spec: &str) -> String {
    spec.replace("Mod+", "CmdOrCtrl+")
}

/// Event name the UI listens to for a menu item: `menu://` plus the id with `.` turned into `/`
/// (`wc.update` -> `menu://wc/update`). Tauri refuses event names with characters other than
/// alphanumerics, `-`, `/`, `:` and `_`, on `emit` and on the UI's `listen` alike.
pub fn event_name(id: &str) -> String {
    format!("menu://{}", id.replace('.', "/"))
}

fn has_item_id(entries: &[Entry], wanted: &str) -> bool {
    entries.iter().any(|e| match e {
        Entry::Item { id, .. } => *id == wanted,
        Entry::Submenu { entries, .. } => has_item_id(entries, wanted),
        _ => false,
    })
}

/// Items of [`menu_spec`] that Rust handles itself instead of emitting `menu://<id>`: they act on
/// the window or the process, which the UI has no permission for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeAction {
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Exit,
}

/// The native handler of a menu item id, if Rust handles it (see [`NativeAction`]).
pub fn native_action(id: &str) -> Option<NativeAction> {
    match id {
        "view.zoom-in" => Some(NativeAction::ZoomIn),
        "view.zoom-out" => Some(NativeAction::ZoomOut),
        "view.zoom-reset" => Some(NativeAction::ZoomReset),
        "app.exit" => Some(NativeAction::Exit),
        _ => None,
    }
}

/// Zoom levels in percent, as browsers step them.
pub const ZOOM_STEPS: [u16; 13] = [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200, 250, 300];

/// The zoom level after `action` at `current` percent (clamped to [`ZOOM_STEPS`]).
pub fn next_zoom(current: u16, action: NativeAction) -> u16 {
    match action {
        NativeAction::ZoomIn => ZOOM_STEPS
            .iter()
            .copied()
            .find(|z| *z > current)
            .unwrap_or(ZOOM_STEPS[ZOOM_STEPS.len() - 1]),
        NativeAction::ZoomOut => ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|z| *z < current)
            .unwrap_or(ZOOM_STEPS[0]),
        NativeAction::ZoomReset | NativeAction::Exit => 100,
    }
}

/// The event to emit for an activated menu item: only items of [`menu_spec`] (on any OS and in
/// either layout variant) that Rust does not handle itself ([`native_action`]) become
/// `menu://<id>` events. Predefined items (copy, paste, quit, ...) and unknown ids emit nothing.
pub fn event_for(id: &str) -> Option<String> {
    if native_action(id).is_some() {
        return None;
    }
    let ours = [Os::Darwin, Os::Windows, Os::Linux].into_iter().any(|os| {
        [true, false]
            .into_iter()
            .any(|l| has_item_id(&menu_spec(os, l), id))
    });
    ours.then(|| event_name(id))
}

/// Current zoom of the app's webviews, in percent.
static ZOOM_PERCENT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(100);

fn run_native<R: tauri::Runtime>(app: &tauri::AppHandle<R>, action: NativeAction) {
    use std::sync::atomic::Ordering;
    use tauri::Manager;
    if action == NativeAction::Exit {
        app.exit(0);
        return;
    }
    let zoom = next_zoom(ZOOM_PERCENT.load(Ordering::Relaxed), action);
    ZOOM_PERCENT.store(zoom, Ordering::Relaxed);
    for w in app.webview_windows().values() {
        if let Err(e) = w.set_zoom(f64::from(zoom) / 100.0) {
            tracing::warn!(error = %e, "failed to set the zoom level");
        }
    }
}

fn predefined<R: tauri::Runtime, M: tauri::Manager<R>>(
    app: &M,
    p: Predef,
) -> tauri::Result<PredefinedMenuItem<R>> {
    match p {
        Predef::Undo => PredefinedMenuItem::undo(app, None),
        Predef::Redo => PredefinedMenuItem::redo(app, None),
        Predef::Cut => PredefinedMenuItem::cut(app, None),
        Predef::Copy => PredefinedMenuItem::copy(app, None),
        Predef::Paste => PredefinedMenuItem::paste(app, None),
        Predef::SelectAll => PredefinedMenuItem::select_all(app, None),
        Predef::Hide => PredefinedMenuItem::hide(app, None),
        Predef::Quit => PredefinedMenuItem::quit(app, None),
        Predef::Minimize => PredefinedMenuItem::minimize(app, None),
        Predef::Zoom => PredefinedMenuItem::maximize(app, None),
        Predef::Fullscreen => PredefinedMenuItem::fullscreen(app, None),
        Predef::BringAllToFront => PredefinedMenuItem::bring_all_to_front(app, None),
        Predef::About => PredefinedMenuItem::about(app, None, None),
    }
}

fn fill<R: tauri::Runtime, M: tauri::Manager<R>>(
    app: &M,
    builder: SubmenuBuilder<'_, R, M>,
    entries: &[Entry],
) -> tauri::Result<Submenu<R>> {
    let mut builder = builder;
    for e in entries {
        builder = match e {
            Entry::Separator => builder.separator(),
            Entry::Predefined(p) => builder.item(&predefined(app, *p)?),
            Entry::Item { id, label, accel } => {
                let mut b = MenuItemBuilder::with_id(*id, *label);
                if let Some(a) = accel {
                    b = b.accelerator(accelerator(a));
                }
                builder.item(&b.build(app)?)
            }
            Entry::Submenu { label, entries } => {
                let inner = fill(app, SubmenuBuilder::new(app, *label), entries)?;
                builder.item(&inner)
            }
        };
    }
    builder.build()
}

/// Builds the native menu bar.
pub fn build_menu<R: tauri::Runtime, M: tauri::Manager<R>>(
    app: &M,
    os: Os,
    has_layout: bool,
) -> tauri::Result<Menu<R>> {
    let mut tops = Vec::new();
    for e in menu_spec(os, has_layout) {
        if let Entry::Submenu { label, entries } = e {
            tops.push(fill(app, SubmenuBuilder::new(app, label), &entries)?);
        }
    }
    let refs: Vec<&dyn tauri::menu::IsMenuItem<R>> = tops
        .iter()
        .map(|s| s as &dyn tauri::menu::IsMenuItem<R>)
        .collect();
    Menu::with_items(app, &refs)
}

/// Installs the menu and forwards activations to the UI as `menu://<id>`.
pub fn install<R: tauri::Runtime>(app: &tauri::App<R>, has_layout: bool) -> tauri::Result<()> {
    let menu = build_menu(app, Os::current(), has_layout)?;
    app.set_menu(menu)?;
    app.on_menu_event(|app, event| {
        if let Some(action) = native_action(&event.id().0) {
            run_native(app, action);
            return;
        }
        let Some(name) = event_for(&event.id().0) else {
            return;
        };
        if let Err(e) = app.emit(&name, ()) {
            tracing::warn!(event = %name, error = %e, "failed to forward menu event");
        }
    });
    Ok(())
}

/// Changes the installed menu while the app runs.
pub trait MenuControl: Send + Sync + 'static {
    /// Switches the Branch / Location variant (the UI calls this when the open working copy
    /// changes).
    fn set_layout_mode(&self, has_layout: bool) -> Result<(), opensvn_ipc::AppError>;
}

/// [`MenuControl`] over a Tauri app handle.
pub struct TauriMenu<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
}

impl<R: tauri::Runtime> TauriMenu<R> {
    /// Wraps an app handle.
    pub fn new(app: tauri::AppHandle<R>) -> Self {
        Self { app }
    }
}

impl<R: tauri::Runtime> MenuControl for TauriMenu<R> {
    fn set_layout_mode(&self, has_layout: bool) -> Result<(), opensvn_ipc::AppError> {
        let menu = build_menu(&self.app, Os::current(), has_layout).map_err(menu_error)?;
        self.app.set_menu(menu).map(|_| ()).map_err(menu_error)
    }
}

fn menu_error(e: tauri::Error) -> opensvn_ipc::AppError {
    opensvn_ipc::AppError::internal("Could not update the menu.").with_detail(e.to_string())
}

#[cfg(test)]
mod tests;
