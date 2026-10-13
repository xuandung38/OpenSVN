/** Keyboard shortcuts of doc 01 section 8. `Mod` is Cmd on macOS and Ctrl elsewhere. */

export function isMac(): boolean {
  return typeof navigator !== "undefined" && /mac|iphone|ipad/i.test(navigator.platform ?? "");
}

export type ShortcutId =
  | "commit"
  | "update"
  | "checkUpdates"
  | "lock"
  | "discard"
  | "showLog"
  | "blame"
  | "checkout"
  | "addExisting"
  | "switchRepo"
  | "viewChanges"
  | "viewHistory"
  | "viewBrowse"
  | "activity"
  | "filter"
  | "toggleDiffMode"
  | "toggleWhitespace"
  | "switch";

interface Spec {
  key: string;
  shift?: boolean;
  alt?: boolean;
}

export const SHORTCUTS: Record<ShortcutId, Spec> = {
  commit: { key: "Enter" },
  update: { key: "u", shift: true },
  checkUpdates: { key: "k", shift: true },
  lock: { key: "l" },
  discard: { key: "Backspace", shift: true },
  showLog: { key: "l", shift: true },
  blame: { key: "b", shift: true },
  checkout: { key: "o", shift: true },
  addExisting: { key: "o" },
  switchRepo: { key: "t" },
  viewChanges: { key: "1" },
  viewHistory: { key: "2" },
  viewBrowse: { key: "3" },
  activity: { key: "j" },
  filter: { key: "f" },
  toggleDiffMode: { key: "d", alt: true },
  toggleWhitespace: { key: "w", alt: true },
  switch: { key: "b" },
};

/** True when the keyboard event is the given shortcut (Mod held, modifiers matching exactly). */
export function matchShortcut(e: KeyboardEvent, id: ShortcutId, mac = isMac()): boolean {
  const s = SHORTCUTS[id];
  const mod = mac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  if (!mod || e.shiftKey !== Boolean(s.shift) || e.altKey !== Boolean(s.alt)) return false;
  if (e.key.toLowerCase() === s.key.toLowerCase()) return true;
  // Option+letter yields a different character on macOS; the physical key code stays stable.
  return Boolean(s.alt) && /^[a-z]$/i.test(s.key) && e.code === `Key${s.key.toUpperCase()}`;
}

/** Display text, e.g. "Ctrl+Shift+U" or "⌘⇧U". */
export function shortcutLabel(id: ShortcutId, mac = isMac()): string {
  const s = SHORTCUTS[id];
  const key = s.key.length === 1 ? s.key.toUpperCase() : s.key === "Backspace" ? "⌫" : s.key;
  if (mac) return `⌘${s.shift ? "⇧" : ""}${s.alt ? "⌥" : ""}${key}`;
  return [
    "Ctrl",
    s.shift ? "Shift" : "",
    s.alt ? "Alt" : "",
    s.key === "Backspace" ? "Backspace" : key,
  ]
    .filter(Boolean)
    .join("+");
}
