import { useEffect, useRef } from "react";
import { useEventBus } from "../../app/providers";
import { useToast } from "../../components/ui/toast";
import { ipc, toAppError, unwrap } from "../../ipc/client";
import { openExternalUrl } from "../../lib/external-link";
import { useT } from "../../lib/i18n/i18n";
import type { ThemeChoice } from "../../lib/theme";
import { DOCS_URL } from "./help/help-links";
import { matchShortcut, SHORTCUTS, type ShortcutId } from "./shortcuts";
import { runUiCommand } from "./ui-commands";
import { useWcActions } from "./use-wc-actions";
import { toggleViewPref, type ViewPref } from "./view-prefs";
import { changedFiles, isConflicted } from "./wc-model";
import { useWcSession } from "./wc-session";
import { wcStrings } from "./wc-strings";

export type MainView = "changes" | "history" | "browse";

/**
 * A native menu item and its accelerator can both reach the WebView for one key press (the menu
 * event and the keydown), in either order: on macOS the keydown usually comes first, on Windows
 * and Linux the menu event can. Whichever arrives second within this window after the same action
 * ran from the other source is the echo and is ignored.
 */
export const MENU_ECHO_MS = 400;

/** What a native menu item does in the UI: a keyboard-shortcut action, or a menu-only one. */
export type MenuAction =
  | ShortcutId
  | "recent"
  | "copyUrl"
  | "updateTo"
  | "discardAll"
  | "cleanup"
  | "sparse"
  | "unlock"
  | "changelist"
  | "ignore"
  | "resolve"
  | "rename"
  | "delete"
  | "conflicts"
  | "showOnServer"
  | "properties"
  | "selProperties"
  | "layout"
  | "themeSystem"
  | "themeLight"
  | "themeDark"
  | "toggleUnversioned"
  | "toggleIgnored"
  | "toggleGroupChangelist"
  | "toggleTree"
  | "compare"
  | "docs"
  | "logsFolder"
  | "reportProblem"
  /** Planned for a later phase: says so instead of doing nothing. */
  | "notYet";

/**
 * Every native menu item event (ids from `src-tauri/src/menu.rs`) and what it runs. Its keys are
 * exactly `MENU_EVENTS` (tested), and a Rust test checks that every menu item is either listed
 * there or handled natively (zoom, Exit, predefined roles), so no item does nothing.
 */
export const MENU_ACTIONS: Readonly<Record<string, MenuAction>> = {
  "menu://file/checkout": "checkout",
  "menu://file/add-wc": "addExisting",
  "menu://file/switch-repo": "switchRepo",
  "menu://file/recent": "recent",
  "menu://file/close-wc": "closeWc",
  "menu://app/settings": "settings",
  "menu://app/check-app-updates": "notYet",
  "menu://edit/find": "filter",
  "menu://edit/copy-path": "copyPath",
  "menu://edit/copy-url": "copyUrl",
  "menu://view/changes": "viewChanges",
  "menu://view/history": "viewHistory",
  "menu://view/browse": "viewBrowse",
  "menu://view/activity": "activity",
  "menu://view/sidebar": "toggleSidebar",
  "menu://view/diff-mode": "toggleDiffMode",
  "menu://view/hide-whitespace": "toggleWhitespace",
  "menu://view/show-unversioned": "toggleUnversioned",
  "menu://view/show-ignored": "toggleIgnored",
  "menu://view/group-changelist": "toggleGroupChangelist",
  "menu://view/tree": "toggleTree",
  "menu://view/theme-system": "themeSystem",
  "menu://view/theme-light": "themeLight",
  "menu://view/theme-dark": "themeDark",
  "menu://wc/check-updates": "checkUpdates",
  "menu://wc/update": "update",
  "menu://wc/update-to": "updateTo",
  "menu://wc/commit": "commit",
  "menu://wc/discard-all": "discardAll",
  "menu://wc/sparse": "sparse",
  "menu://wc/cleanup": "cleanup",
  "menu://wc/properties": "properties",
  "menu://wc/layout": "layout",
  "menu://wc/upgrade": "notYet",
  "menu://wc/reveal": "reveal",
  "menu://wc/terminal": "terminal",
  "menu://wc/editor": "editor",
  "menu://branch/switch": "switch",
  "menu://branch/new": "newBranch",
  "menu://branch/new-tag": "newTag",
  "menu://branch/merge": "notYet",
  "menu://branch/compare": "compare",
  "menu://branch/show-on-server": "showOnServer",
  "menu://branch/layout": "layout",
  "menu://sel/discard": "discard",
  "menu://sel/lock": "lock",
  "menu://sel/unlock": "unlock",
  "menu://sel/changelist": "changelist",
  "menu://sel/ignore": "ignore",
  "menu://sel/resolve": "resolve",
  "menu://sel/log": "showLog",
  "menu://sel/blame": "blame",
  "menu://sel/rename": "rename",
  "menu://sel/delete": "delete",
  "menu://sel/properties": "selProperties",
  "menu://window/conflicts": "conflicts",
  "menu://help/docs": "docs",
  "menu://help/shortcuts": "showShortcuts",
  "menu://help/logs": "logsFolder",
  "menu://help/report": "reportProblem",
};

/** View menu toggles: the pref each flips and its label in the confirmation toast. */
const VIEW_TOGGLES = {
  toggleUnversioned: ["showUnversioned", "viewShowUnversioned"],
  toggleIgnored: ["showIgnored", "viewShowIgnored"],
  toggleGroupChangelist: ["groupByChangelist", "viewGroupChangelist"],
  toggleTree: ["tree", "viewTree"],
} as const satisfies Record<string, readonly [ViewPref, keyof (typeof wcStrings)["en"]]>;

/**
 * App-level keyboard shortcuts (doc 01 section 8) and every native-menu event. A key press and
 * the native accelerator of the same item can both reach the WebView; whichever comes second
 * within `MENU_ECHO_MS` is dropped, so each action runs once. Selection-scoped actions apply to the
 * selected file. Component-owned actions (filter, diff toggles) go through `ui-commands`. The
 * latest session/action values are read through a ref so the listeners are registered once.
 */
export function useWcShortcuts(props: {
  onView: (v: MainView) => void;
  onToggleActivity: () => void;
  onAddExisting: () => void;
  /** Opens the "Current repository" picker (Mod+T). */
  onSwitchRepo?: () => void;
  onOpenSettings?: () => void;
  onToggleSidebar?: () => void;
  /** Closes the current working copy (it stays in the repository list). */
  onCloseWc?: () => void;
  onCreate?: (kind: "branch" | "tag" | "location") => void;
  onTheme?: (theme: ThemeChoice) => void;
  onShowShortcuts?: () => void;
}): void {
  const session = useWcSession();
  const actions = useWcActions();
  const bus = useEventBus();
  const toast = useToast();
  const t = useT(wcStrings);
  const latest = useRef({ session, actions, toast, t, ...props });
  latest.current = { session, actions, toast, t, ...props };

  useEffect(() => {
    // When each action last ran from the keyboard and from the native menu.
    const lastKey = new Map<MenuAction, number>();
    const lastMenu = new Map<MenuAction, number>();
    const isEcho = (from: Map<MenuAction, number>, id: MenuAction) =>
      Date.now() - (from.get(id) ?? Number.NEGATIVE_INFINITY) < MENU_ECHO_MS;
    const run = (id: MenuAction): boolean => {
      const { session: s, actions: a, toast, t, ...p } = latest.current;
      const wc = s.wc;
      const sel = s.files.find((f) => f.path === s.selected);
      const selPaths = sel ? [sel.path] : [];
      const versioned = sel && sel.text !== "unversioned" && sel.text !== "ignored";
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
        case "updateTo":
          if (!wc) return false;
          s.openDialog({ kind: "update-rev" });
          return true;
        case "discardAll": {
          const changed = changedFiles(s.files);
          if (!wc || changed.length === 0) return false;
          s.openDialog({ kind: "discard", paths: changed.map((f) => f.path), all: true });
          return true;
        }
        case "cleanup":
          if (!wc) return false;
          s.openDialog({ kind: "cleanup" });
          return true;
        case "sparse":
          if (!wc) return false;
          s.openDialog({ kind: "sparse" });
          return true;
        case "properties":
          // The working copy itself (its root folder).
          if (!wc) return false;
          s.openDialog({ kind: "properties", paths: [""] });
          return true;
        case "selProperties":
          if (!wc || !versioned) return false;
          s.openDialog({ kind: "properties", paths: selPaths });
          return true;
        case "layout":
          if (!wc) return false;
          s.openDialog({ kind: "layout" });
          return true;
        case "conflicts":
          if (!wc) return false;
          s.openDialog({ kind: "resolver", path: null });
          return true;
        case "lock":
          if (!versioned) return false;
          s.openDialog({ kind: "lock", mode: "lock", paths: selPaths });
          return true;
        case "unlock":
          if (!sel?.lock) return false;
          s.openDialog({ kind: "lock", mode: "unlock", paths: selPaths });
          return true;
        case "discard":
          if (!sel || sel.text === "normal" || sel.text === "unversioned") return false;
          s.openDialog({ kind: "discard", paths: selPaths, all: false });
          return true;
        case "changelist":
          if (!versioned) return false;
          s.openDialog({ kind: "changelist", paths: selPaths });
          return true;
        case "ignore":
          if (sel?.text !== "unversioned") return false;
          s.openDialog({ kind: "ignore", path: sel.path });
          return true;
        case "resolve":
          if (!sel || !isConflicted(sel)) return false;
          s.openDialog({ kind: "resolver", path: sel.path });
          return true;
        case "rename":
          if (!versioned) return false;
          s.openDialog({ kind: "rename", path: sel.path });
          return true;
        case "delete":
          if (!versioned) return false;
          s.openDialog({ kind: "delete", paths: selPaths });
          return true;
        case "showLog":
          if (!sel || !s.links.showLog) return false;
          s.links.showLog(sel.path);
          return true;
        case "blame":
          if (!sel || !s.links.blame) return false;
          s.links.blame(sel.path);
          return true;
        case "copyPath":
          if (!wc) return false;
          void a.copyPath(sel?.path ?? "", false);
          return true;
        case "copyUrl":
          if (!wc) return false;
          void a.copyUrl(sel?.path ?? "");
          return true;
        case "reveal":
          if (!wc) return false;
          void a.reveal("");
          return true;
        case "terminal":
        case "editor":
          if (!wc) return false;
          void a.openWith("", id);
          return true;
        case "checkout":
          s.openDialog({ kind: "checkout" });
          return true;
        case "addExisting":
          p.onAddExisting();
          return true;
        case "switchRepo":
        case "recent":
          if (!p.onSwitchRepo) return false;
          p.onSwitchRepo();
          return true;
        case "closeWc":
          if (!wc || !p.onCloseWc) return false;
          p.onCloseWc();
          return true;
        case "settings":
          if (!p.onOpenSettings) return false;
          p.onOpenSettings();
          return true;
        case "switch":
          if (!wc) return false;
          s.openDialog({ kind: "switch" });
          return true;
        case "newBranch":
          if (!wc || !p.onCreate) return false;
          p.onCreate(wc.branch ? "branch" : "location");
          return true;
        case "newTag":
          if (!wc?.branch || !p.onCreate) return false;
          p.onCreate("tag");
          return true;
        case "showOnServer":
          if (!wc) return false;
          p.onView("browse");
          return true;
        case "viewChanges":
          p.onView("changes");
          return true;
        case "viewHistory":
          p.onView("history");
          return true;
        case "viewBrowse":
          p.onView("browse");
          return true;
        case "activity":
          p.onToggleActivity();
          return true;
        case "toggleSidebar":
          if (!p.onToggleSidebar) return false;
          p.onToggleSidebar();
          return true;
        case "themeSystem":
        case "themeLight":
        case "themeDark":
          if (!p.onTheme) return false;
          p.onTheme(id === "themeSystem" ? "system" : id === "themeLight" ? "light" : "dark");
          return true;
        case "showShortcuts":
          if (!p.onShowShortcuts) return false;
          p.onShowShortcuts();
          return true;
        case "filter":
        case "toggleDiffMode":
        case "toggleWhitespace":
          return runUiCommand(id);
        case "toggleUnversioned":
        case "toggleIgnored":
        case "toggleGroupChangelist":
        case "toggleTree": {
          // Native menu items carry no check mark: the toast says what the toggle did.
          const [pref, label] = VIEW_TOGGLES[id];
          const on = toggleViewPref(pref);
          toast.show(t(on ? "viewOn" : "viewOff", { name: t(label) }));
          return true;
        }
        case "compare":
          if (!wc) return false;
          s.openDialog({ kind: "compare" });
          return true;
        case "docs":
          openExternalUrl(DOCS_URL).catch((e: unknown) =>
            toast.show(t("docsFailed", { message: toAppError(e).message })),
          );
          return true;
        case "logsFolder":
          unwrap(ipc().commands.logsReveal()).catch((e: unknown) =>
            toast.show(t("logsFailed", { message: toAppError(e).message })),
          );
          return true;
        case "reportProblem":
          s.openDialog({ kind: "report" });
          return true;
        case "notYet":
          toast.show(t("menuNotYet"));
          return true;
      }
    };

    const keyIds = Object.keys(SHORTCUTS) as ShortcutId[];
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      // Dialogs are modal: shortcuts must not fire behind them.
      if (document.querySelector("dialog[open]")) return;
      for (const id of keyIds) {
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
      const id = MENU_ACTIONS[name];
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
