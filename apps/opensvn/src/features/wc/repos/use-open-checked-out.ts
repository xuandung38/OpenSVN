import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { useEventBus } from "../../../app/providers";
import { useToast } from "../../../components/ui/toast";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { repoStrings } from "./repo-strings";

/**
 * When a checkout finishes (also one sent to the background), the new working copy is opened and
 * becomes the current one, like a fresh clone in GitHub Desktop.
 */
export function useOpenCheckedOut(onSelect: (wcId: string) => void): void {
  const bus = useEventBus();
  const qc = useQueryClient();
  const toast = useToast();
  const r = useT(repoStrings);
  const latest = useRef({ onSelect, r, toast });
  latest.current = { onSelect, r, toast };

  useEffect(
    () =>
      bus.on("op://finished", (p) => {
        if (p.result.kind !== "checkedOut" || !p.result.wcId) {
          if (p.result.kind === "checkedOut") {
            void qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
          }
          return;
        }
        const wcId = p.result.wcId;
        void (async () => {
          const { onSelect: select, r: tr, toast: t } = latest.current;
          try {
            const wc = await unwrap(ipc().commands.wcOpen(wcId));
            await qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
            select(wcId);
            t.show(tr("checkedOut", { name: wc.name }));
          } catch (e) {
            t.show(toAppError(e).message, 6000);
          }
          void qc.invalidateQueries({ queryKey: queryKeys.recentUrls });
        })();
      }),
    [bus, qc],
  );
}
