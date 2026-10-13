import { memo } from "react";
import { cn } from "../../lib/cn";
import type { StatusCode } from "../../lib/status";
import { LockBadge } from "../ui/lock-badge";
import { SafeText } from "../ui/safe-text";
import { StatusGlyph } from "../ui/status-glyph";

export interface FileRowProps {
  /** Repo-relative path; the directory part renders muted, the name in fg. */
  path: string;
  status: StatusCode;
  checked?: boolean;
  selected?: boolean;
  lock?: "me" | "other";
  onToggle?: (path: string) => void;
  onSelect?: (path: string) => void;
}

/** Working-copy file row: grid `18px 20px 1fr auto` (design-guidelines 6). Paths render as text only. */
export const FileRow = memo(function FileRow({
  path,
  status,
  checked = false,
  selected = false,
  lock,
  onToggle,
  onSelect,
}: FileRowProps) {
  const slash = path.lastIndexOf("/");
  const dir = slash >= 0 ? path.slice(0, slash + 1) : "";
  const name = slash >= 0 ? path.slice(slash + 1) : path;
  return (
    <div
      role="option"
      aria-selected={selected}
      tabIndex={-1}
      onClick={() => onSelect?.(path)}
      onKeyDown={(e) => {
        if (e.key === "Enter") onSelect?.(path);
        if (e.key === " ") {
          e.preventDefault();
          onToggle?.(path);
        }
      }}
      className={cn(
        "grid h-full cursor-default grid-cols-[18px_20px_1fr_auto] items-center gap-1.5 px-2.5 py-[5px]",
        selected ? "bg-sel" : "hover:bg-surface-2",
      )}
    >
      <input
        type="checkbox"
        checked={checked}
        aria-label={`Include ${path}`}
        onClick={(e) => e.stopPropagation()}
        onChange={() => onToggle?.(path)}
        className="m-0 h-[14px] w-[14px] accent-[var(--accent)]"
      />
      <StatusGlyph code={status} />
      <span className="min-w-0 truncate font-mono text-[12px]" title={path}>
        <SafeText as="span" className="text-muted">
          {dir}
        </SafeText>
        <SafeText as="span" className="text-fg">
          {name}
        </SafeText>
      </span>
      {lock ? <LockBadge owner={lock} /> : <span />}
    </div>
  );
});
