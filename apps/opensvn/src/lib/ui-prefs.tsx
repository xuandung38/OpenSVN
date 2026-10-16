import { createContext, type ReactNode, useContext, useEffect, useMemo } from "react";
import type { Density, Settings } from "../ipc/bindings";

/** Size the layout was designed at (`body` font size, row heights). */
export const BASE_FONT_SIZE = 13;

export interface UiPrefs {
  fontSize: number;
  density: Density;
  tabWidth: number;
}

export const DEFAULT_UI_PREFS: UiPrefs = {
  fontSize: BASE_FONT_SIZE,
  density: "comfortable",
  tabWidth: 4,
};

const UiPrefsContext = createContext<UiPrefs>(DEFAULT_UI_PREFS);

/**
 * Height of a list row designed as `base` px at 13 px comfortable: it grows with the font size, and
 * compact density takes 15% off while still fitting one line of text.
 */
export function rowHeightFor(base: number, prefs: UiPrefs): number {
  const scaled = Math.round((base * prefs.fontSize) / BASE_FONT_SIZE);
  if (prefs.density !== "compact") return scaled;
  return Math.max(Math.round(scaled * 0.85), Math.ceil(prefs.fontSize * 1.4) + 2);
}

/** Current appearance preferences (defaults outside the provider). */
export function useUiPrefs(): UiPrefs {
  return useContext(UiPrefsContext);
}

/** `rowHeightFor` with the current preferences (defaults outside the provider, e.g. in tests). */
export function useRowHeight(base: number): number {
  return rowHeightFor(base, useContext(UiPrefsContext));
}

/** Writes the preferences to the document: CSS variables (CSSOM, allowed by the CSP) and `data-density`. */
export function applyUiPrefs(prefs: UiPrefs, root: HTMLElement = document.documentElement): void {
  root.style.setProperty("--ui-font-size", `${prefs.fontSize}px`);
  root.style.setProperty("--tab-width", String(prefs.tabWidth));
  root.setAttribute("data-density", prefs.density);
}

/** Saved appearance settings, or the defaults until they are loaded. */
export function uiPrefsOf(s: Settings | undefined): UiPrefs {
  return s
    ? {
        fontSize: s["appearance.fontSize"],
        density: s["appearance.density"],
        tabWidth: s["diff.tabWidth"],
      }
    : DEFAULT_UI_PREFS;
}

/** Applies the saved appearance settings to the document and to list row heights. */
export function UiPrefsProvider({
  settings,
  children,
}: {
  settings: Settings | undefined;
  children: ReactNode;
}) {
  const { fontSize, density, tabWidth } = uiPrefsOf(settings);
  const prefs = useMemo(() => ({ fontSize, density, tabWidth }), [fontSize, density, tabWidth]);
  useEffect(() => applyUiPrefs(prefs), [prefs]);
  return <UiPrefsContext.Provider value={prefs}>{children}</UiPrefsContext.Provider>;
}
