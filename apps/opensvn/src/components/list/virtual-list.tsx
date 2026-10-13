import { useVirtualizer } from "@tanstack/react-virtual";
import { type KeyboardEvent, type ReactNode, type Ref, useId, useState } from "react";
import { cn } from "../../lib/cn";

export interface RowContext {
  /** Stable DOM id; the listbox points `aria-activedescendant` at it. */
  id: string;
  /** Keyboard-active row (not necessarily the selected one). */
  active: boolean;
  /** Called by the row on mouse click so keyboard and pointer share one active index. */
  activate: () => void;
}

/**
 * Windowed list for file/revision rows (TanStack Virtual). Fixed row height; only the visible window
 * (+ overscan) is mounted, so 100k rows stay cheap.
 *
 * Keyboard model (ARIA listbox, `aria-activedescendant`): the listbox itself is the single tab stop;
 * Up/Down/Home/End/PageUp/PageDown move the active row and scroll it into view even when the row is
 * not mounted; Enter calls `onActivate`, Space calls `onToggle`. Rows never take focus, so nothing is
 * lost when the virtualizer unmounts them. Rows position via CSSOM `style` (allowed by `style-src 'self'`).
 */
export function VirtualList({
  count,
  rowHeight,
  renderRow,
  label,
  className,
  scrollRef,
  onActivate,
  onToggle,
}: {
  count: number;
  rowHeight: number;
  renderRow: (index: number, ctx: RowContext) => ReactNode;
  label: string;
  className?: string;
  /** Optional handle to the scroll element (scripted scroll in the perf spike). */
  scrollRef?: Ref<HTMLDivElement>;
  /** Enter on the active row. */
  onActivate?: (index: number) => void;
  /** Space on the active row (e.g. toggle its checkbox). */
  onToggle?: (index: number) => void;
}) {
  const [parent, setParent] = useState<HTMLDivElement | null>(null);
  const [active, setActive] = useState(0);
  const baseId = useId();
  const optionId = (i: number) => `${baseId}-opt-${i}`;

  const virt = useVirtualizer({
    count,
    getScrollElement: () => parent,
    estimateSize: () => rowHeight,
    overscan: 12,
  });

  const move = (next: number) => {
    const i = Math.max(0, Math.min(count - 1, next));
    setActive(i);
    virt.scrollToIndex(i);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (count === 0) return;
    const page = Math.max(1, Math.floor((parent?.clientHeight ?? rowHeight * 10) / rowHeight) - 1);
    switch (e.key) {
      case "ArrowDown":
        move(active + 1);
        break;
      case "ArrowUp":
        move(active - 1);
        break;
      case "PageDown":
        move(active + page);
        break;
      case "PageUp":
        move(active - page);
        break;
      case "Home":
        move(0);
        break;
      case "End":
        move(count - 1);
        break;
      case "Enter":
        onActivate?.(active);
        break;
      case " ":
        onToggle?.(active);
        break;
      default:
        return;
    }
    e.preventDefault();
  };

  const activeIndex = Math.min(active, Math.max(0, count - 1));

  return (
    <div
      ref={(node) => {
        setParent(node);
        if (typeof scrollRef === "function") scrollRef(node);
        else if (scrollRef) (scrollRef as { current: HTMLDivElement | null }).current = node;
      }}
      role="listbox"
      tabIndex={0}
      aria-label={label}
      aria-multiselectable="true"
      aria-activedescendant={count > 0 ? optionId(activeIndex) : undefined}
      onKeyDown={onKeyDown}
      className={cn("group overflow-auto", className)}
    >
      <div style={{ height: virt.getTotalSize(), position: "relative" }}>
        {virt.getVirtualItems().map((v) => (
          <div
            key={v.key}
            style={{
              position: "absolute",
              top: 0,
              left: 0,
              right: 0,
              height: v.size,
              transform: `translateY(${v.start}px)`,
            }}
          >
            {renderRow(v.index, {
              id: optionId(v.index),
              active: v.index === activeIndex,
              activate: () => setActive(v.index),
            })}
          </div>
        ))}
      </div>
    </div>
  );
}
