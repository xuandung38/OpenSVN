import { Popover as P } from "radix-ui";
import type { ReactNode } from "react";

/** 320px popover (Working copy / Branch pickers). Radix Popover is non-modal by default. */
export function Popover({ trigger, children }: { trigger: ReactNode; children: ReactNode }) {
  return (
    <P.Root>
      <P.Trigger asChild>{trigger}</P.Trigger>
      <P.Portal>
        <P.Content
          align="start"
          sideOffset={4}
          className="z-[6] flex w-80 flex-col gap-1.5 rounded-lg border border-line bg-surface p-2 text-fg shadow-float"
        >
          {children}
        </P.Content>
      </P.Portal>
    </P.Root>
  );
}

/** Eyebrow group label inside a popover list. */
export function PopoverGroup({ children }: { children: ReactNode }) {
  return (
    <div className="px-1.5 pt-1.5 text-[10px] font-medium tracking-[.07em] text-muted uppercase">
      {children}
    </div>
  );
}
