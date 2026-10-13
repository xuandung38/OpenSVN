import type { ContextMenu, DropdownMenu } from "radix-ui";
import { MENU_ITEM, MENU_SEPARATOR } from "./menu-styles";

export interface MenuEntry {
  label: string;
  shortcut?: string;
  disabled?: boolean;
  onSelect?: () => void;
}
export type MenuEntries = (MenuEntry | "separator")[];

type Kit = typeof DropdownMenu | typeof ContextMenu;

/** Renders entries with the Radix primitives of either DropdownMenu or ContextMenu. */
export function renderEntries(kit: Kit, entries: MenuEntries) {
  return entries.map((e, i) =>
    e === "separator" ? (
      // biome-ignore lint/suspicious/noArrayIndexKey: static separator list
      <kit.Separator key={i} className={MENU_SEPARATOR} />
    ) : (
      <kit.Item
        key={e.label}
        disabled={e.disabled}
        onSelect={() => e.onSelect?.()}
        className={MENU_ITEM}
      >
        <span>{e.label}</span>
        {e.shortcut && <kbd className="font-mono text-[11px] opacity-70">{e.shortcut}</kbd>}
      </kit.Item>
    ),
  );
}
