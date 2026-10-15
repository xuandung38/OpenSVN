import { Check } from "lucide-react";
import { cn } from "../../lib/cn";

/**
 * Tick box drawn inside a listbox option. It is a mouse affordance only: the option exposes the state
 * through `aria-checked` and the listbox toggles it with Space. A native checkbox would be a focusable
 * control nested in the option (axe `nested-interactive`), so this is a plain, hidden element.
 */
export function RowCheck({
  checked,
  onToggle,
  onPointerActivate,
}: {
  checked: boolean;
  onToggle?: () => void;
  onPointerActivate?: () => void;
}) {
  return (
    // Pointer only (hidden from assistive tech): keyboard toggling is Space on the owning listbox.
    <span
      aria-hidden="true"
      data-row-check={checked ? "checked" : "unchecked"}
      onClick={(e) => {
        e.stopPropagation();
        onPointerActivate?.();
        onToggle?.();
      }}
      className={cn(
        "grid h-[14px] w-[14px] cursor-default place-items-center rounded-[3px] border",
        checked ? "border-accent bg-accent text-on-accent" : "border-muted bg-surface",
      )}
    >
      {checked && <Check size={11} strokeWidth={3} />}
    </span>
  );
}
