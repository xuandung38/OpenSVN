import { type ReactNode, useEffect, useRef } from "react";
import { cn } from "../../lib/cn";

/**
 * Modal built on the native <dialog>: focus trap, Esc, inert background and top-layer stacking come
 * from the browser, so no script-injected <style> (Radix Dialog's scroll lock) is needed under
 * `style-src 'self'`. Click on the backdrop closes.
 */
export function ModalDialog({
  open,
  onClose,
  label,
  className,
  children,
}: {
  open: boolean;
  onClose: () => void;
  /** Accessible name. */
  label: string;
  className?: string;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  // Latest `open` for the native close handler (it fires after the prop already flipped).
  const openRef = useRef(open);
  openRef.current = open;
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (open && !el.open) el.showModal();
    if (!open && el.open) el.close();
  }, [open]);
  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents: backdrop click is a pointer shortcut; keyboard close is native Esc (cancel event)
    <dialog
      ref={ref}
      aria-label={label}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === ref.current) onClose();
      }}
      // Native close without a cancelable `cancel` (e.g. CloseWatcher): sync parent state once.
      onClose={() => {
        if (openRef.current) onClose();
      }}
      className={cn(
        "m-auto overflow-hidden border border-line bg-surface p-0 text-fg shadow-float",
        className,
      )}
    >
      {open && children}
    </dialog>
  );
}
