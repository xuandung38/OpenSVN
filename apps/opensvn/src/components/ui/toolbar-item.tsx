import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "../../lib/cn";

export interface ToolbarItemProps extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "value"> {
  icon: ReactNode;
  label: string;
  value: ReactNode;
  /** Pill after the value, e.g. incoming revisions "↓3". */
  badge?: string;
  /** Spins the icon (disabled under prefers-reduced-motion). */
  busy?: boolean;
}

/** Two-line toolbar button: small muted label over a bold value (design-guidelines 6). */
export function ToolbarItem({
  icon,
  label,
  value,
  badge,
  busy,
  className,
  ...rest
}: ToolbarItemProps) {
  return (
    <button
      type="button"
      className={cn(
        "flex min-w-[220px] cursor-pointer items-center gap-2.5 border-0 border-r border-line bg-transparent px-3.5 py-2 text-left hover:bg-surface-2",
        className,
      )}
      {...rest}
    >
      <span className={cn("w-[18px] font-mono text-muted", busy && "spin")} aria-hidden="true">
        {icon}
      </span>
      <span className="flex flex-col">
        <small className="text-[11px] text-muted">{label}</small>
        <b className="font-semibold">
          {value}
          {badge && (
            <span className="tnum ml-1.5 rounded-[9px] bg-accent px-1.5 text-[11px] text-on-accent">
              {badge}
            </span>
          )}
        </b>
      </span>
    </button>
  );
}
