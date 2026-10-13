/** Shared class names for DropdownMenu and ContextMenu (design-guidelines 6: dropdown menu). */
export const MENU_CONTENT =
  "z-[6] min-w-[270px] rounded-lg border border-line bg-surface p-[5px] text-fg shadow-float";
export const MENU_ITEM =
  "group flex cursor-pointer items-center justify-between gap-6 rounded-[5px] px-[9px] py-[5px] whitespace-nowrap outline-none " +
  "data-[highlighted]:bg-accent data-[highlighted]:text-on-accent data-[disabled]:cursor-default data-[disabled]:bg-transparent data-[disabled]:text-muted-text";
export const MENU_SEPARATOR = "mx-0.5 my-1 h-px bg-line";
