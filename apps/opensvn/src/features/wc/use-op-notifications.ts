import { useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import { useEventBus } from "../../app/providers";
import { useToast } from "../../components/ui/toast";
import type { OpState } from "../../ipc/events";
import { queryKeys } from "../../ipc/query-keys";
import { useT } from "../../lib/i18n/i18n";
import { wcStrings } from "./wc-strings";

/**
 * Toasts for operation outcomes the user did not wait on: failures (except user cancellation) and
 * finished updates/switches. Commit results are reported by the commit box itself.
 */
export function useOpNotifications(): void {
  const bus = useEventBus();
  const qc = useQueryClient();
  const toast = useToast();
  const t = useT(wcStrings);

  useEffect(() => {
    const lookup = (opId: string) =>
      qc.getQueryData<Record<string, OpState>>(queryKeys.ops)?.[opId]?.handle;
    const offFailed = bus.on("op://failed", (p) => {
      if (p.error.code === "cancelled") return;
      const title = lookup(p.opId)?.title ?? "";
      toast.show(t("opFailed", { title, message: p.error.message }).trim(), 6000);
    });
    const offDone = bus.on("op://finished", (p) => {
      const kind = lookup(p.opId)?.kind;
      if ((kind === "update" || kind === "switch") && p.result.kind === "newRev") {
        toast.show(t("updatedTo", { rev: p.result.newRev }));
      }
    });
    return () => {
      offFailed();
      offDone();
    };
  }, [bus, qc, toast, t]);
}
