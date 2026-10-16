import { useSyncExternalStore } from "react";

/**
 * View > Show unversioned / Show ignored / Group by changelist / Show as tree (doc 01 8.4). They
 * shape the Changes list only and are remembered per user in local storage, so they survive a
 * restart; storage that is missing or blocked just means the defaults.
 */
export interface ViewPrefs {
  showUnversioned: boolean;
  showIgnored: boolean;
  groupByChangelist: boolean;
  tree: boolean;
}

export type ViewPref = keyof ViewPrefs;

export const DEFAULT_VIEW_PREFS: Readonly<ViewPrefs> = {
  showUnversioned: true,
  showIgnored: false,
  groupByChangelist: true,
  tree: false,
};

const KEY = "opensvn.viewPrefs";

function storage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

/** Stored prefs over the defaults; unknown keys and non-boolean values are ignored. */
export function parseViewPrefs(raw: string | null | undefined): ViewPrefs {
  const out = { ...DEFAULT_VIEW_PREFS };
  if (!raw) return out;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed && typeof parsed === "object") {
      for (const k of Object.keys(DEFAULT_VIEW_PREFS) as ViewPref[]) {
        const v = (parsed as Record<string, unknown>)[k];
        if (typeof v === "boolean") out[k] = v;
      }
    }
  } catch {
    // Corrupt value: defaults.
  }
  return out;
}

function load(): ViewPrefs {
  try {
    return parseViewPrefs(storage()?.getItem(KEY));
  } catch {
    return { ...DEFAULT_VIEW_PREFS };
  }
}

let current: ViewPrefs = load();
const listeners = new Set<() => void>();

export function getViewPrefs(): ViewPrefs {
  return current;
}

export function setViewPref(key: ViewPref, value: boolean): void {
  if (current[key] === value) return;
  current = { ...current, [key]: value };
  try {
    storage()?.setItem(KEY, JSON.stringify(current));
  } catch {
    // Storage full or blocked: the choice still holds for this session.
  }
  for (const l of [...listeners]) l();
}

/** Flips one pref and returns its new value. */
export function toggleViewPref(key: ViewPref): boolean {
  const next = !current[key];
  setViewPref(key, next);
  return next;
}

/** Back to the stored value (tests reset between cases with an empty storage). */
export function reloadViewPrefs(): void {
  current = load();
  for (const l of [...listeners]) l();
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function useViewPrefs(): ViewPrefs {
  return useSyncExternalStore(subscribe, getViewPrefs, getViewPrefs);
}
