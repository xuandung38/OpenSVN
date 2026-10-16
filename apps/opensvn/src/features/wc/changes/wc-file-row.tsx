import { memo } from "react";
import { LockBadge } from "../../../components/ui/lock-badge";
import { RowCheck } from "../../../components/ui/row-check";
import { SafeText } from "../../../components/ui/safe-text";
import { StatusGlyph } from "../../../components/ui/status-glyph";
import { cn } from "../../../lib/cn";
import type { StatusCode } from "../../../lib/status";
import { displayPath } from "../wc-model";

export interface WcFileRowProps {
  id: string;
  path: string;
  /** Text shown for the working copy root (path `""`). */
  rootLabel: string;
  glyph: StatusCode;
  checked: boolean;
  selected: boolean;
  active: boolean;
  lock?: "me" | "other";
  needsLock: boolean;
  outOfDate: boolean;
  /** Tree view level: the row then shows the file name only, indented. */
  depth?: number;
  /** False for rows listed for reference only (ignored files): no commit checkbox. */
  tickable?: boolean;
  /** Localised labels for the non-colour badges. */
  labels: { needsLock: string; outOfDate: string };
  onToggle: (path: string) => void;
  onSelect: (path: string) => void;
  onPointerActivate: () => void;
}

/**
 * Changes row: checkbox, status glyph, path (dir muted, name strong), badges. Like the shared
 * `FileRow` it is a listbox option driven by `aria-activedescendant`, but it also shows the
 * needs-lock and out-of-date markers. The path is always rendered as text.
 */
export const WcFileRow = memo(function WcFileRow(p: WcFileRowProps) {
  const shown = displayPath(p.path, p.rootLabel);
  const slash = p.path.lastIndexOf("/");
  const dir = slash >= 0 ? shown.slice(0, slash + 1) : "";
  const name = slash >= 0 ? shown.slice(slash + 1) : shown;
  const tree = p.depth !== undefined;
  const tickable = p.tickable !== false;
  return (
    // biome-ignore lint/a11y/useFocusableInteractive: roving focus lives on the owning listbox (aria-activedescendant)
    // biome-ignore lint/a11y/useKeyWithClickEvents: keyboard activation is handled by the listbox (Enter/Space)
    <div
      id={p.id}
      role="option"
      aria-selected={p.selected}
      aria-checked={tickable ? p.checked : undefined}
      onClick={() => {
        p.onPointerActivate();
        p.onSelect(p.path);
      }}
      onContextMenu={() => {
        p.onPointerActivate();
        p.onSelect(p.path);
      }}
      className={cn(
        "grid h-full cursor-default grid-cols-[18px_20px_1fr_auto] items-center gap-1.5 px-2.5 py-[5px]",
        p.selected ? "bg-sel" : "hover:bg-surface-2",
        !tickable && "text-muted-text",
        p.active &&
          "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
      )}
    >
      {tickable ? (
        <RowCheck
          checked={p.checked}
          onToggle={() => p.onToggle(p.path)}
          onPointerActivate={p.onPointerActivate}
        />
      ) : (
        <span />
      )}
      <StatusGlyph code={p.glyph} />
      <span
        className="min-w-0 truncate font-mono text-[12px]"
        title={shown}
        style={tree ? { paddingLeft: (p.depth ?? 0) * 14 } : undefined}
      >
        {!tree && (
          <SafeText as="span" className="text-muted-text">
            {dir}
          </SafeText>
        )}
        <SafeText as="span" className={tickable ? "text-fg" : "text-muted-text"}>
          {name}
        </SafeText>
      </span>
      <span className="flex items-center gap-1.5 text-[11px]">
        {p.outOfDate && (
          <span title={p.labels.outOfDate} className="text-info-text">
            <span aria-hidden="true">↓</span>
            <span className="sr-only">{p.labels.outOfDate}</span>
          </span>
        )}
        {p.needsLock && !p.lock && (
          <span className="rounded-[4px] border border-line px-1 text-muted-text">
            {p.labels.needsLock}
          </span>
        )}
        {p.lock && <LockBadge owner={p.lock} />}
      </span>
    </div>
  );
});
