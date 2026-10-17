import { memo } from "react";
import { cn } from "../../lib/cn";
import { SafeText } from "../ui/safe-text";

export interface RevisionRowProps {
  rev: number;
  message: string;
  author: string;
  /** Pre-formatted relative time ("2h ago"). */
  when: string;
  /** Revision exists on the server but is not yet in the working copy. */
  incoming?: boolean;
  selected?: boolean;
  /** Short tag before the message (e.g. the revision marked for a comparison). */
  badge?: string;
  /** Nesting level (revisions a merge brought in sit below the merging one). */
  indent?: number;
  /** Second-line note after the time (e.g. which revision merged this one in). */
  note?: string;
  /** Keyboard-active row of the owning listbox and its DOM id (see VirtualList). */
  active?: boolean;
  id?: string;
  onSelect?: (rev: number) => void;
  onPointerActivate?: () => void;
}

/** Two-line revision row: first message line (ellipsis) + `rN · author · when` meta. */
export const RevisionRow = memo(function RevisionRow({
  rev,
  message,
  author,
  when,
  incoming = false,
  selected = false,
  badge,
  indent = 0,
  note,
  active = false,
  id,
  onSelect,
  onPointerActivate,
}: RevisionRowProps) {
  const firstLine = message.split("\n", 1)[0] ?? "";
  return (
    // biome-ignore lint/a11y/useFocusableInteractive: roving focus lives on the owning listbox (aria-activedescendant)
    // biome-ignore lint/a11y/useKeyWithClickEvents: keyboard activation is handled by the listbox (Enter/Space)
    <div
      id={id}
      role="option"
      aria-selected={selected}
      onClick={() => {
        onPointerActivate?.();
        onSelect?.(rev);
      }}
      style={indent > 0 ? { paddingLeft: `${10 + indent * 16}px` } : undefined}
      className={cn(
        "flex h-full cursor-default flex-col justify-center px-2.5 py-[5px]",
        indent > 0 && "border-l-2 border-line",
        selected ? "bg-sel" : "hover:bg-surface-2",
        active &&
          "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
      )}
    >
      <span className="truncate">
        {incoming && <span className="mr-1 text-accent-text">↓ not updated</span>}
        {badge && (
          <span className="mr-1 rounded-sm bg-surface-2 px-1 text-[11px] text-info-text">
            {badge}
          </span>
        )}
        <SafeText>{firstLine}</SafeText>
      </span>
      <span className="tnum truncate text-[11px] text-muted-text">
        <span className="font-mono">r{rev}</span> · <SafeText>{author}</SafeText> · {when}
        {note && <> · {note}</>}
      </span>
    </div>
  );
});
