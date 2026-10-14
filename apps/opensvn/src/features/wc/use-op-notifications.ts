import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { useEventBus } from "../../app/providers";
import { useToast } from "../../components/ui/toast";
import type { LockConflict } from "../../ipc/bindings";
import { type OpState, PHASE_NESTED_WC_SKIPPED } from "../../ipc/events";
import { queryKeys } from "../../ipc/query-keys";
import { useT } from "../../lib/i18n/i18n";
import { wcStrings } from "./wc-strings";

/**
 * Toasts for operation outcomes the user did not wait on: failures (except user cancellation),
 * finished updates/switches, and nested working copies an add skipped (svn's W150002). Commit
 * results are reported by the commit box itself.
 *
 * A failed lock of working copy files whose error carries the `open-lock-dialog` action and the
 * refusing locks is handed to `onLockConflict`, which reopens the lock dialog with the owner and
 * the Steal option (doc 01 §10.4) when it is no longer open.
 */
export function useOpNotifications(
  onLockConflict?: (wcId: string, conflicts: LockConflict[]) => void,
): void {
  const bus = useEventBus();
  const qc = useQueryClient();
  const toast = useToast();
  const t = useT(wcStrings);
  const lockConflict = useRef(onLockConflict);
  lockConflict.current = onLockConflict;

  useEffect(() => {
    const lookup = (opId: string) =>
      qc.getQueryData<Record<string, OpState>>(queryKeys.ops)?.[opId]?.handle;
    const offFailed = bus.on("op://failed", (p) => {
      if (p.error.code === "cancelled") return;
      const handle = lookup(p.opId);
      const title = handle?.title ?? "";
      toast.show(t("opFailed", { title, message: p.error.message }).trim(), 6000);
      const conflicts = p.error.lockConflicts ?? [];
      if (
        handle?.kind === "lock" &&
        handle.wcId &&
        conflicts.length > 0 &&
        p.error.actions?.includes("open-lock-dialog")
      ) {
        lockConflict.current?.(handle.wcId, conflicts);
      }
    });
    const offDone = bus.on("op://finished", (p) => {
      const kind = lookup(p.opId)?.kind;
      if ((kind === "update" || kind === "switch") && p.result.kind === "newRev") {
        toast.show(t("updatedTo", { rev: p.result.newRev }));
      }
    });
    const offNotice = bus.on("op://progress", (p) => {
      if (p.phase === PHASE_NESTED_WC_SKIPPED && p.currentPath) {
        toast.show(t("nestedWcSkipped", { detail: p.currentPath }), 8000);
      }
    });
    return () => {
      offFailed();
      offDone();
      offNotice();
    };
  }, [bus, qc, toast, t]);
}
