import { Tabs as T } from "radix-ui";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

export interface TabDef {
  id: string;
  label: string;
  count?: number;
}

/** Equal-width tabs; the selected one is fg/600 with a 2px accent underline. Content is rendered by the caller. */
export function Tabs({
  tabs,
  value,
  onValueChange,
  children,
  label,
}: {
  tabs: TabDef[];
  value: string;
  onValueChange: (id: string) => void;
  children?: ReactNode;
  label: string;
}) {
  return (
    <T.Root value={value} onValueChange={onValueChange} className="flex min-h-0 flex-col">
      <T.List aria-label={label} className="flex border-b border-line">
        {tabs.map((t) => (
          <T.Trigger
            key={t.id}
            value={t.id}
            className={cn(
              "-mb-px flex-1 cursor-pointer border-0 border-b-2 border-transparent bg-transparent px-2 py-[7px] text-muted",
              "data-[state=active]:border-accent data-[state=active]:font-semibold data-[state=active]:text-fg",
            )}
          >
            {t.label}
            {t.count !== undefined && (
              <span className="tnum ml-1.5 rounded-[9px] bg-surface-2 px-1.5 text-[11px] text-muted">
                {t.count}
              </span>
            )}
          </T.Trigger>
        ))}
      </T.List>
      {children}
    </T.Root>
  );
}
