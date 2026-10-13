import { Popover as P } from "radix-ui";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

/**
 * 320px popover (Working copy / Branch pickers). Radix Popover is non-modal by default. Pass
 * `open`/`onOpenChange` to control it (e.g. opened from a keyboard shortcut).
 */
export function Popover({
  trigger,
  children,
  open,
  onOpenChange,
  className,
  label,
}: {
  trigger: ReactNode;
  children: ReactNode;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  className?: string;
  /** Accessible name of the popup. */
  label?: string;
}) {
  return (
    <P.Root open={open} onOpenChange={onOpenChange}>
      <P.Trigger asChild>{trigger}</P.Trigger>
      <P.Portal>
        <P.Content
          align="start"
          sideOffset={4}
          aria-label={label}
          className={cn(
            "z-[6] flex w-80 flex-col gap-1.5 rounded-lg border border-line bg-surface p-2 text-fg shadow-float",
            className,
          )}
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
    <div className="px-1.5 pt-1.5 text-[10px] font-medium tracking-[.07em] text-muted-text uppercase">
      {children}
    </div>
  );
}
