import { useVirtualizer } from "@tanstack/react-virtual";
import { type ReactNode, type Ref, useRef } from "react";
import { cn } from "../../lib/cn";

/**
 * Windowed list for file/revision rows (TanStack Virtual). Rows are fixed height; only the visible
 * window (+ overscan) is mounted, so 100k rows stay cheap. Rows position via CSSOM `style`
 * (allowed by `style-src 'self'`; only inline style *attributes in markup* are blocked).
 */
export function VirtualList({
  count,
  rowHeight,
  renderRow,
  label,
  className,
  scrollRef,
}: {
  count: number;
  rowHeight: number;
  renderRow: (index: number) => ReactNode;
  label: string;
  className?: string;
  /** Optional handle to the scroll element (scripted scroll in the perf spike). */
  scrollRef?: Ref<HTMLDivElement>;
}) {
  const parent = useRef<HTMLDivElement | null>(null);
  const virt = useVirtualizer({
    count,
    getScrollElement: () => parent.current,
    estimateSize: () => rowHeight,
    overscan: 12,
  });
  return (
    <div
      ref={(node) => {
        parent.current = node;
        if (typeof scrollRef === "function") scrollRef(node);
        else if (scrollRef) (scrollRef as { current: HTMLDivElement | null }).current = node;
      }}
      role="listbox"
      aria-label={label}
      aria-multiselectable="true"
      className={cn("overflow-auto", className)}
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
            {renderRow(v.index)}
          </div>
        ))}
      </div>
    </div>
  );
}
