import type { ReactNode } from "react";
import { ModalDialog } from "./modal-dialog";

/**
 * 480px dialog (640px when `wide`): header 15px/600 with bottom border, body, right-aligned footer
 * (Cancel first).
 */
export function Dialog({
  open,
  onClose,
  title,
  children,
  footer,
  wide = false,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
}) {
  return (
    <ModalDialog
      open={open}
      onClose={onClose}
      label={title}
      className={`${wide ? "w-[640px]" : "w-[480px]"} max-w-[92%] rounded-[10px]`}
    >
      <header className="border-b border-line px-4 py-3 text-[15px] font-semibold">{title}</header>
      <div className="flex flex-col gap-2.5 px-4 py-3.5">{children}</div>
      {footer && (
        <footer className="flex justify-end gap-2 border-t border-line bg-surface-2 px-4 py-2.5">
          {footer}
        </footer>
      )}
    </ModalDialog>
  );
}

/** Info blocks inside dialogs. */
export function WarnBox({ children }: { children: ReactNode }) {
  return <div className="rounded-md bg-warn-soft px-2.5 py-2">{children}</div>;
}
export function BadBox({ children }: { children: ReactNode }) {
  return <div className="rounded-md bg-bad-soft px-2.5 py-2">{children}</div>;
}
