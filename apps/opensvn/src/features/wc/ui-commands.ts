import { useEffect, useRef } from "react";

/**
 * Actions owned by a component that is not always mounted (the file filter, the diff toolbar).
 * The keyboard/menu router (`use-wc-shortcuts.ts`) runs them through this channel so one place
 * sees both the key press and the native menu event and can drop the echo.
 */
export type UiCommand = "filter" | "toggleDiffMode" | "toggleWhitespace";

const handlers = new Map<UiCommand, Set<() => void>>();

/** Runs every mounted handler of `id`; false when none is mounted (nothing happened). */
export function runUiCommand(id: UiCommand): boolean {
  const set = handlers.get(id);
  if (!set || set.size === 0) return false;
  for (const h of [...set]) h();
  return true;
}

/** Registers `handler` for `id` while the calling component is mounted (latest closure wins). */
export function useUiCommand(id: UiCommand, handler: () => void): void {
  const latest = useRef(handler);
  latest.current = handler;
  useEffect(() => {
    const set = handlers.get(id) ?? new Set<() => void>();
    handlers.set(id, set);
    const h = () => latest.current();
    set.add(h);
    return () => {
      set.delete(h);
    };
  }, [id]);
}
