import { Tabs as T } from "radix-ui";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

export interface TabDef {
  id: string;
  label: string;
  count?: number;
}

/**
 * Equal-width tabs; the selected one is fg/600 with a 2px accent underline. Every tab has a panel
 * element so `aria-controls` always resolves; only the active panel renders its children.
 */
export function Tabs({
  tabs,
  value,
  onValueChange,
  panels,
  label,
  className,
}: {
  tabs: TabDef[];
  value: string;
  onValueChange: (id: string) => void;
  /** Panel content by tab id (rendered only while that tab is active). */
  panels?: Record<string, ReactNode>;
  label: string;
  className?: string;
}) {
  return (
    <T.Root
      value={value}
      onValueChange={onValueChange}
      className={cn("flex min-h-0 flex-col", className)}
    >
      <T.List aria-label={label} className="flex border-b border-line">
        {tabs.map((t) => (
          <T.Trigger
            key={t.id}
            value={t.id}
            className={cn(
              "-mb-px flex-1 cursor-pointer border-0 border-b-2 border-transparent bg-transparent px-2 py-[7px] text-muted-text",
              "data-[state=active]:border-accent data-[state=active]:font-semibold data-[state=active]:text-fg",
            )}
          >
            {t.label}
            {t.count !== undefined && (
              <span className="tnum ml-1.5 rounded-[9px] bg-surface-2 px-1.5 text-[11px] text-muted-text">
                {t.count}
              </span>
            )}
          </T.Trigger>
        ))}
      </T.List>
      {tabs.map((t) => (
        <T.Content
          key={t.id}
          value={t.id}
          forceMount
          className="min-h-0 flex-1 flex-col data-[state=active]:flex data-[state=inactive]:hidden"
        >
          {t.id === value ? panels?.[t.id] : null}
        </T.Content>
      ))}
    </T.Root>
  );
}
