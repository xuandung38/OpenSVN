import type { ReactNode } from "react";
import { ModalDialog } from "./modal-dialog";

/** Large sheet (Conflict resolver, Settings): inset 18px from the window edge, header/body/footer grid. */
export function Sheet({
  open,
  onClose,
  title,
  children,
  footer,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
  footer?: ReactNode;
}) {
  return (
    <ModalDialog
      open={open}
      onClose={onClose}
      label={title}
      className="h-[calc(100%-36px)] w-[calc(100%-36px)] max-w-none rounded-[10px] open:grid open:grid-rows-[auto_1fr_auto]"
    >
      <header className="border-b border-line px-4 py-3 text-[15px] font-semibold">{title}</header>
      <div className="min-h-0 overflow-auto">{children}</div>
      <footer className="flex justify-end gap-2 border-t border-line bg-surface-2 px-4 py-2.5">
        {footer}
      </footer>
    </ModalDialog>
  );
}
