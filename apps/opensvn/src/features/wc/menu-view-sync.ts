import { useEffect } from "react";
import type { MenuViewState } from "../../ipc/bindings";
import { ipc } from "../../ipc/client";
import { getViewPrefs, useViewPrefs, type ViewPrefs } from "./view-prefs";

/** The check marks of the native View menu for `prefs`. */
export function menuViewState(prefs: ViewPrefs): MenuViewState {
  return {
    showUnversioned: prefs.showUnversioned,
    showIgnored: prefs.showIgnored,
    groupByChangelist: prefs.groupByChangelist,
    tree: prefs.tree,
  };
}

/**
 * Keeps the check marks of View › Show unversioned files / Show ignored files / Group by
 * changelist / Show as tree in the native menu in step with the stored choices: once at start-up
 * and on every change, whether it came from the menu, the Changes list options or a shortcut. A
 * failure (no menu bar) changes nothing in the app, so it is ignored.
 */
export function useMenuViewSync(): void {
  const prefs = useViewPrefs();
  const { showUnversioned, showIgnored, groupByChangelist, tree } = prefs;
  useEffect(() => {
    const view = { showUnversioned, showIgnored, groupByChangelist, tree };
    void Promise.resolve()
      .then(() => ipc().commands.menuSetView(view))
      .catch(() => {});
  }, [showUnversioned, showIgnored, groupByChangelist, tree]);
}

/**
 * Sends the stored choices to the native menu again. A check item flips its own mark when it is
 * clicked, so a click the app does not act on (a dialog is open, or the same key press already
 * ran the toggle) would leave the mark inverted; this puts it back.
 */
export function resendMenuView(): void {
  void Promise.resolve()
    .then(() => ipc().commands.menuSetView(menuViewState(getViewPrefs())))
    .catch(() => {});
}
