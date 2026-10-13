import { DropdownMenu as M } from "radix-ui";
import type { ReactNode } from "react";
import { type MenuEntries, renderEntries } from "./menu-entries";
import { MENU_CONTENT } from "./menu-styles";

export type { MenuEntries, MenuEntry } from "./menu-entries";

/**
 * Dropdown menu. `modal={false}` on purpose: Radix modal mode injects a <style> element
 * (react-remove-scroll) which the production CSP (`style-src 'self'`) would block.
 */
export function DropdownMenu({ trigger, entries }: { trigger: ReactNode; entries: MenuEntries }) {
  return (
    <M.Root modal={false}>
      <M.Trigger asChild>{trigger}</M.Trigger>
      <M.Portal>
        <M.Content align="start" sideOffset={4} className={MENU_CONTENT}>
          {renderEntries(M, entries)}
        </M.Content>
      </M.Portal>
    </M.Root>
  );
}
