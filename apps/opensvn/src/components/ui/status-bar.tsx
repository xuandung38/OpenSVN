import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

export function StatusBar({ children }: { children: ReactNode }) {
  return (
    <footer className="flex items-center gap-4 border-t border-line bg-surface-2 px-3.5 py-1 text-[12px] whitespace-nowrap text-muted">
      {children}
    </footer>
  );
}

/** Single status-bar entry. `tone` colours connection state; the text itself always states it. */
export function StatusBarItem({ tone, children }: { tone?: "ok" | "bad"; children: ReactNode }) {
  return (
    <span className={cn(tone === "ok" && "text-ok", tone === "bad" && "text-bad")}>{children}</span>
  );
}
