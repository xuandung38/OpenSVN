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
  /** Keyboard-active row of the owning listbox (see VirtualList). */
  active?: boolean;
  /** DOM id referenced by the listbox's aria-activedescendant. */
  id?: string;
  lock?: "me" | "other";
  onToggle?: (path: string) => void;
  onSelect?: (path: string) => void;
  /** Called on pointer interaction so the listbox's active row follows the mouse. */
  onPointerActivate?: () => void;
}

/**
 * Working-copy file row: grid `18px 20px 1fr auto` (design-guidelines 6). Paths render as text only.
 * The row is an `option` and is never focusable itself; the checkbox is a mouse affordance whose state
 * is exposed through the option's `aria-checked` (keyboard: Space on the listbox).
 */
export const FileRow = memo(function FileRow({
  path,
  status,
  checked = false,
  selected = false,
  active = false,
  id,
  lock,
  onToggle,
  onSelect,
  onPointerActivate,
}: FileRowProps) {
  const slash = path.lastIndexOf("/");
  const dir = slash >= 0 ? path.slice(0, slash + 1) : "";
  const name = slash >= 0 ? path.slice(slash + 1) : path;
  return (
    // biome-ignore lint/a11y/useFocusableInteractive: roving focus lives on the owning listbox (aria-activedescendant)
    // biome-ignore lint/a11y/useKeyWithClickEvents: keyboard activation is handled by the listbox (Enter/Space)
    <div
      id={id}
      role="option"
      aria-selected={selected}
      aria-checked={checked}
      onClick={() => {
        onPointerActivate?.();
        onSelect?.(path);
      }}
      className={cn(
        "grid h-full cursor-default grid-cols-[18px_20px_1fr_auto] items-center gap-1.5 px-2.5 py-[5px]",
        selected ? "bg-sel" : "hover:bg-surface-2",
        active &&
          "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
      )}
    >
      <input
        type="checkbox"
        tabIndex={-1}
        aria-hidden="true"
        checked={checked}
        onClick={(e) => {
          e.stopPropagation();
          onPointerActivate?.();
        }}
        onChange={() => onToggle?.(path)}
        className="m-0 h-[14px] w-[14px] accent-[var(--accent)]"
      />
      <StatusGlyph code={status} />
      <span className="min-w-0 truncate font-mono text-[12px]" title={path}>
        <SafeText as="span" className="text-muted-text">
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
