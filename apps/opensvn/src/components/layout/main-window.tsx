import type { ReactNode } from "react";
import { cn } from "../../lib/cn";
import { ResizableSidebar } from "./resizable-sidebar";

/**
 * Window grid (design-guidelines 5): toolbar / banner slot / [sidebar | main] / activity / status bar.
 * Slots are plain props so the shell stays presentational and P12 can fill `main`.
 */
export function MainWindow({
  toolbar,
  banner,
  sidebar,
  main,
  activity,
  statusBar,
  className,
}: {
  toolbar: ReactNode;
  banner?: ReactNode;
  /** Left column; `undefined` hides it (View > Toggle sidebar). */
  sidebar?: ReactNode;
  main: ReactNode;
  activity?: ReactNode;
  statusBar: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex h-full min-h-[600px] min-w-[960px] flex-col bg-surface", className)}>
      {toolbar}
      {banner}
      <div className="flex min-h-0 flex-1">
        {sidebar !== undefined && <ResizableSidebar>{sidebar}</ResizableSidebar>}
        <main className="min-w-0 flex-1 overflow-auto">{main}</main>
      </div>
      {activity}
      {statusBar}
    </div>
  );
}
