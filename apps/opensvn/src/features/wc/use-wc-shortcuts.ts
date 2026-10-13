import { useEffect, useRef } from "react";
import { useEventBus } from "../../app/providers";
import { matchShortcut, type ShortcutId } from "./shortcuts";
import { useWcActions } from "./use-wc-actions";
import { useWcSession } from "./wc-session";

export type MainView = "changes" | "history" | "browse";

/**
 * App-level keyboard shortcuts (doc 01 section 8) and the native-menu events that mirror them.
 * Selection-scoped actions (lock, discard, log, blame) apply to the selected file. The latest
 * session/action values are read through a ref so the listeners are registered once.
 */
export function useWcShortcuts({
  onView,
  onToggleActivity,
  onAddExisting,
}: {
  onView: (v: MainView) => void;
  onToggleActivity: () => void;
  onAddExisting: () => void;
}): void {
  const session = useWcSession();
  const actions = useWcActions();
  const bus = useEventBus();
  const latest = useRef({ session, actions, onView, onToggleActivity, onAddExisting });
  latest.current = { session, actions, onView, onToggleActivity, onAddExisting };

  useEffect(() => {
    const run = (id: ShortcutId): boolean => {
      const { session: s, actions: a, onView, onToggleActivity, onAddExisting } = latest.current;
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
        if (matchShortcut(e, id) && run(id)) {
          e.preventDefault();
          return;
        }
      }
    };
    window.addEventListener("keydown", onKey);

    const off = bus.onMenu((name) => {
      const map: Record<string, ShortcutId | undefined> = {
        "menu://commit": "commit",
        "menu://update": "update",
        "menu://refresh": "checkUpdates",
        "menu://add-working-copy": "addExisting",
      };
      const id = map[name];
      if (id) run(id);
    });
    return () => {
      window.removeEventListener("keydown", onKey);
      off();
    };
  }, [bus]);
}
