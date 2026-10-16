import { createContext, type ReactNode, useContext, useMemo, useState } from "react";
import type { FileStatus, LockConflict, WorkingCopy } from "../../ipc/bindings";

/** Dialogs and panes the working-copy screens can open. Paths are working-copy relative. */
export type WcDialog =
  | { kind: "checkout" }
  | { kind: "discard"; paths: string[]; all: boolean }
  | { kind: "ignore"; path: string }
  | { kind: "rename"; path: string }
  | { kind: "delete"; paths: string[] }
  | {
      kind: "lock";
      mode: "lock" | "unlock";
      paths: string[];
      /** Locks known to refuse the request (reopened after a failed lock). */
      conflicts?: LockConflict[];
    }
  | { kind: "changelist"; paths: string[] }
  | { kind: "update-rev" }
  | { kind: "switch" }
  | {
      kind: "cleanup";
      /** Opened from the "in use by another program" banner: the lock option is shown first. */
      foreignLock?: boolean;
    }
  | { kind: "properties"; paths: string[] }
  | { kind: "layout" }
  | { kind: "resolver"; path: string | null }
  | { kind: "compare" }
  | { kind: "report" }
  | { kind: "repo-rename"; wcId: string; name: string }
  | { kind: "repo-remove"; wcId: string; name: string };

/** Dialogs that do not belong to the current working copy (they survive switching to another). */
export function isAppDialog(d: WcDialog | null): boolean {
  return (
    d?.kind === "checkout" ||
    d?.kind === "repo-rename" ||
    d?.kind === "repo-remove" ||
    d?.kind === "report"
  );
}

export interface WcLinks {
  /** Show the history of a working-copy path (History tab). */
  showLog?: (path: string) => void;
  /** Show blame of a working-copy path at its base revision (Browse tab). */
  blame?: (path: string) => void;
}

export interface WcSession {
  wc: WorkingCopy | null;
  /** Every status entry of the working copy (including `normal` ones). */
  files: readonly FileStatus[];
  selected: string | null;
  setSelected: (path: string | null) => void;
  /** Paths ticked for the next commit. */
  checked: ReadonlySet<string>;
  setChecked: (next: ReadonlySet<string>) => void;
  dialog: WcDialog | null;
  openDialog: (d: WcDialog) => void;
  closeDialog: () => void;
  links: WcLinks;
}

const SessionContext = createContext<WcSession | null>(null);

/**
 * Selection, commit ticks and the open dialog of the current working copy. They reset when the
 * working copy changes (without remounting the children, so the rest of the window keeps its state).
 */
export function WcSessionProvider({
  wc,
  files,
  links = {},
  children,
}: {
  wc: WorkingCopy | null;
  files: readonly FileStatus[];
  links?: WcLinks;
  children: ReactNode;
}) {
  const wcId = wc?.id ?? null;
  const [scope, setScope] = useState(wcId);
  const [selected, setSelected] = useState<string | null>(null);
  const [checked, setChecked] = useState<ReadonlySet<string>>(new Set());
  const [dialog, setDialog] = useState<WcDialog | null>(null);
  if (scope !== wcId) {
    // Adjusting state while rendering is the supported way to reset on a prop change.
    setScope(wcId);
    setSelected(null);
    setChecked(new Set());
    setDialog((d) => (isAppDialog(d) ? d : null));
  }
  const value = useMemo<WcSession>(
    () => ({
      wc,
      files,
      selected,
      setSelected,
      checked,
      setChecked,
      dialog,
      openDialog: setDialog,
      closeDialog: () => setDialog(null),
      links,
    }),
    [wc, files, selected, checked, dialog, links],
  );
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useWcSession(): WcSession {
  const ctx = useContext(SessionContext);
  if (!ctx) throw new Error("useWcSession must be used inside <WcSessionProvider>");
  return ctx;
}
