import {
  type KeyboardEvent,
  type PointerEvent,
  type ReactNode,
  useCallback,
  useState,
} from "react";

export const SIDEBAR_MIN = 220;
export const SIDEBAR_MAX = 600;
export const SIDEBAR_DEFAULT = 330;

export function clampSidebar(w: number): number {
  return Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, Math.round(w)));
}

/** Sidebar + drag handle (pointer and keyboard operable, ARIA separator). Width 220-600, default 330. */
export function ResizableSidebar({ children }: { children: ReactNode }) {
  const [width, setWidth] = useState(SIDEBAR_DEFAULT);

  const onPointerDown = useCallback((e: PointerEvent<HTMLDivElement>) => {
    e.currentTarget.setPointerCapture(e.pointerId);
  }, []);
  const onPointerMove = useCallback((e: PointerEvent<HTMLDivElement>) => {
    if (!e.currentTarget.hasPointerCapture(e.pointerId)) return;
    const left = e.currentTarget.parentElement?.getBoundingClientRect().left ?? 0;
    setWidth(clampSidebar(e.clientX - left));
  }, []);
  const onKeyDown = useCallback((e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.shiftKey ? 40 : 10;
    if (e.key === "ArrowLeft") setWidth((w) => clampSidebar(w - step));
    else if (e.key === "ArrowRight") setWidth((w) => clampSidebar(w + step));
    else if (e.key === "Home") setWidth(SIDEBAR_MIN);
    else if (e.key === "End") setWidth(SIDEBAR_MAX);
    else return;
    e.preventDefault();
  }, []);

  return (
    <>
      <aside
        aria-label="Sidebar"
        className="flex min-h-0 shrink-0 flex-col border-r border-line bg-surface"
        style={{ width }}
      >
        {children}
      </aside>
      {/* biome-ignore lint/a11y/useSemanticElements: <hr> cannot take focus/keyboard handlers as a splitter */}
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
        aria-valuemin={SIDEBAR_MIN}
        aria-valuemax={SIDEBAR_MAX}
        aria-valuenow={width}
        tabIndex={0}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onKeyDown={onKeyDown}
        className="-ml-0.5 w-1 shrink-0 cursor-col-resize hover:bg-accent-soft"
      />
    </>
  );
}
