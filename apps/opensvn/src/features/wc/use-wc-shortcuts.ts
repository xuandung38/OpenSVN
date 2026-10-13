import { useEffect, useRef } from "react";
import { useEventBus } from "../../app/providers";
import { matchShortcut, type ShortcutId } from "./shortcuts";
import { useWcActions } from "./use-wc-actions";
import { useWcSession } from "./wc-session";

export type MainView = "changes" | "history" | "browse";

/**
 * A native menu item and its accelerator can both reach the WebView for one key press (the menu
 * event and the keydown), in either order: on macOS the keydown usually comes first, on Windows
 * and Linux the menu event can. Whichever arrives second within this window after the same action
 * ran from the other source is the echo and is ignored.
 */
export const MENU_ECHO_MS = 400;

/**
 * App-level keyboard shortcuts (doc 01 section 8) and the native-menu events that mirror them.
 * Selection-scoped actions (lock, discard, log, blame) apply to the selected file. The latest
 * session/action values are read through a ref so the listeners are registered once.
 */
export function useWcShortcuts({
  onView,
  onToggleActivity,
  onAddExisting,
  onSwitchRepo,
  onOpenSettings,
}: {
  onView: (v: MainView) => void;
  onToggleActivity: () => void;
  onAddExisting: () => void;
  /** Opens the "Current repository" picker (Mod+T). */
  onSwitchRepo?: () => void;
  onOpenSettings?: () => void;
}): void {
  const session = useWcSession();
  const actions = useWcActions();
  const bus = useEventBus();
  const props = { onView, onToggleActivity, onAddExisting, onSwitchRepo, onOpenSettings };
  const latest = useRef({ session, actions, ...props });
  latest.current = { session, actions, ...props };

  useEffect(() => {
    // When each action last ran from the keyboard and from the native menu.
    const lastKey = new Map<ShortcutId, number>();
    const lastMenu = new Map<ShortcutId, number>();
    const isEcho = (from: Map<ShortcutId, number>, id: ShortcutId) =>
      Date.now() - (from.get(id) ?? Number.NEGATIVE_INFINITY) < MENU_ECHO_MS;
    const run = (id: ShortcutId): boolean => {
      const {
        session: s,
        actions: a,
        onView,
        onToggleActivity,
        onAddExisting,
        onSwitchRepo,
      } = latest.current;
      const sel = s.files.find((f) => f.path === s.selected);
      const selPaths = sel ? [sel.path] : [];
      switch (id) {
        case "commit":
          document.querySelector<HTMLButtonElement>("[data-wc-commit]")?.click();
          return true;
        case "update":
          void a.update(null);
          return true;
        case "checkUpdates":
          void a.checkForUpdates();
          return true;
        case "lock":
          if (selPaths.length === 0) return false;
          s.openDialog({ kind: "lock", mode: "lock", paths: selPaths });
          return true;
        case "discard":
          if (!sel || sel.text === "normal" || sel.text === "unversioned") return false;
          s.openDialog({ kind: "discard", paths: selPaths, all: false });
          return true;
        case "showLog":
          if (!sel || !s.links.showLog) return false;
          s.links.showLog(sel.path);
          return true;
        case "blame":
          if (!sel || !s.links.blame) return false;
          s.links.blame(sel.path);
          return true;
        case "checkout":
          s.openDialog({ kind: "checkout" });
          return true;
        case "addExisting":
          onAddExisting();
          return true;
        case "switchRepo":
          if (!onSwitchRepo) return false;
          onSwitchRepo();
          return true;
        case "switch":
          if (!s.wc) return false;
          s.openDialog({ kind: "switch" });
          return true;
        case "viewChanges":
          onView("changes");
          return true;
        case "viewHistory":
          onView("history");
          return true;
        case "viewBrowse":
          onView("browse");
          return true;
        case "activity":
          onToggleActivity();
          return true;
        default:
          return false; // filter and diff toggles are handled where their controls live
      }
    };

    const ids: ShortcutId[] = [
      "commit",
      "update",
      "checkUpdates",
      "lock",
      "discard",
      "showLog",
      "blame",
      "checkout",
      "addExisting",
      "switchRepo",
      "switch",
      "viewChanges",
      "viewHistory",
      "viewBrowse",
      "activity",
    ];
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      // Dialogs are modal: shortcuts must not fire behind them.
      if (document.querySelector("dialog[open]")) return;
      for (const id of ids) {
        if (!matchShortcut(e, id)) continue;
        if (isEcho(lastMenu, id)) {
          // The menu already ran it for this key press.
          lastMenu.delete(id);
          e.preventDefault();
          return;
        }
        if (run(id)) {
          lastKey.set(id, Date.now());
          e.preventDefault();
          return;
        }
      }
    };
    window.addEventListener("keydown", onKey);

    const off = bus.onMenu((name) => {
      if (document.querySelector("dialog[open]")) return;
      if (name === "menu://settings") {
        latest.current.onOpenSettings?.();
        return;
      }
      const map: Record<string, ShortcutId | undefined> = {
        "menu://commit": "commit",
        "menu://update": "update",
        "menu://refresh": "checkUpdates",
        "menu://add-working-copy": "addExisting",
        "menu://checkout": "checkout",
        "menu://switch-repository": "switchRepo",
      };
      const id = map[name];
      if (!id) return;
      if (isEcho(lastKey, id)) {
        lastKey.delete(id);
        return;
      }
      if (run(id)) lastMenu.set(id, Date.now());
    });
    return () => {
      window.removeEventListener("keydown", onKey);
      off();
    };
  }, [bus]);
}
