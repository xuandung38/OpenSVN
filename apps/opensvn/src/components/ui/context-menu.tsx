import { ContextMenu as M } from "radix-ui";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";
import { type MenuEntries, renderEntries } from "./menu-entries";
import { MENU_CONTENT } from "./menu-styles";

/** Right-click menu around `children`. Non-modal for the same CSP reason as DropdownMenu. */
export function ContextMenu({ children, entries }: { children: ReactNode; entries: MenuEntries }) {
  return (
    <M.Root modal={false}>
      <M.Trigger asChild>{children}</M.Trigger>
      <M.Portal>
        <M.Content className={cn(MENU_CONTENT, "z-[8]")}>{renderEntries(M, entries)}</M.Content>
      </M.Portal>
    </M.Root>
  );
}
