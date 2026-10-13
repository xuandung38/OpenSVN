import type { ButtonHTMLAttributes } from "react";
import { cn } from "../../lib/cn";

/** Filter chip: 11px pill, accent-soft when on (no border). */
export function Chip({
  on = false,
  className,
  ...rest
}: { on?: boolean } & ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button
      type="button"
      aria-pressed={on}
      className={cn(
        "cursor-pointer rounded-[10px] border px-2 py-0.5 text-[11px]",
        on
          ? "border-transparent bg-accent-soft text-accent"
          : "border-line bg-transparent text-muted",
        className,
      )}
      {...rest}
    />
  );
}
