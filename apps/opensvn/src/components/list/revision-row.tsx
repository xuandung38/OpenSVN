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
  onSelect?: (rev: number) => void;
}

/** Two-line revision row: first message line (ellipsis) + `rN · author · when` meta. */
export const RevisionRow = memo(function RevisionRow({
  rev,
  message,
  author,
  when,
  incoming = false,
  selected = false,
  onSelect,
}: RevisionRowProps) {
  const firstLine = message.split("\n", 1)[0] ?? "";
  return (
    <div
      role="option"
      aria-selected={selected}
      tabIndex={-1}
      onClick={() => onSelect?.(rev)}
      onKeyDown={(e) => e.key === "Enter" && onSelect?.(rev)}
      className={cn(
        "flex h-full cursor-default flex-col justify-center px-2.5 py-[5px]",
        selected ? "bg-sel" : "hover:bg-surface-2",
      )}
    >
      <span className="truncate">
        {incoming && <span className="mr-1 text-accent">↓ not updated</span>}
        <SafeText>{firstLine}</SafeText>
      </span>
      <span className="tnum truncate text-[11px] text-muted">
        <span className="font-mono">r{rev}</span> · <SafeText>{author}</SafeText> · {when}
      </span>
    </div>
  );
});
