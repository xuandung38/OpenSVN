import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

/** Full-width banner under the toolbar: warn (default) or danger, title + text left, actions right. */
export function Banner({
  tone = "warn",
  title,
  children,
  actions,
}: {
  tone?: "warn" | "danger";
  title: string;
  children?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div
      role={tone === "danger" ? "alert" : "status"}
      className={cn(
        "flex items-center gap-2.5 border-b border-line px-3.5 py-[7px] text-fg",
        tone === "danger" ? "bg-bad-soft" : "bg-warn-soft",
      )}
    >
      <b className={tone === "danger" ? "text-bad" : "text-warn"}>{title}</b>
      <span className="min-w-0 flex-1 truncate">{children}</span>
      {actions && <span className="flex gap-1.5">{actions}</span>}
    </div>
  );
}
