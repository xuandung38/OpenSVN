import { useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";
import { useToast } from "../../../components/ui/toast";
import type { OperationHandle } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";

export interface ResumeActions {
  /** Finishes the interrupted checkout or update at the revision it was reaching. */
  continueWc(wcId: string): Promise<OperationHandle | undefined>;
  /**
   * Updates to HEAD instead (also finishes the incomplete folders), with the choices of the
   * interrupted checkout (externals) or switch (its URL), which the backend reads from its journal.
   */
  updateLatest(wcId: string): Promise<OperationHandle | undefined>;
}

/** Continue / "Update to latest instead" for an incomplete working copy; failures go to a toast. */
export function useResumeActions(): ResumeActions {
  const toast = useToast();
  const qc = useQueryClient();
  return useMemo<ResumeActions>(() => {
    const run = async (call: () => Promise<OperationHandle>) => {
      try {
        return await call();
      } catch (e) {
        toast.show(toAppError(e).message, 6000);
        return undefined;
      } finally {
        void qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
      }
    };
    return {
      continueWc: (wcId) => run(() => unwrap(ipc().commands.wcContinue(wcId))),
      updateLatest: (wcId) => run(() => unwrap(ipc().commands.wcUpdateLatest(wcId))),
    };
  }, [toast, qc]);
}
