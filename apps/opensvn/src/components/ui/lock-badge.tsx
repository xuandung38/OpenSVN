import { Lock } from "lucide-react";
import { cn } from "../../lib/cn";

/** Lock indicator: accent for own lock, bad for someone else's. Text label keeps it non-colour-only. */
export function LockBadge({ owner, className }: { owner: "me" | "other"; className?: string }) {
  const label = owner === "me" ? "Locked by you" : "Locked by another user";
  return (
    <span
      title={label}
      className={cn(
        "inline-flex items-center gap-1 text-[11px]",
        owner === "me" ? "text-accent-text" : "text-bad-text",
        className,
      )}
    >
      <Lock size={12} aria-hidden="true" />
      <span>{owner === "me" ? "you" : "other"}</span>
      <span className="sr-only">{label}</span>
    </span>
  );
}
