import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { useToast } from "../../components/ui/toast";
import { ipc, unwrap } from "../../ipc/client";
import { useSettings } from "../../ipc/queries";
import { queryKeys } from "../../ipc/query-keys";
import { useT } from "../../lib/i18n/i18n";
import { autoCheckDue, readLastAutoCheck, writeLastAutoCheck } from "./update-model";
import { updateStrings } from "./update-strings";

/** How long the "new version" notice stays (ms): longer than a plain toast, still not blocking. */
const NOTICE_MS = 10_000;

/**
 * The start-up check: once per app start, at most once a day, only when `updates.autoCheck` is on
 * and a token is stored (no token: nothing is asked, not even the backend's "not configured").
 * A new version shows as a toast; failures stay silent (the user can check from the menu).
 */
export function useAutoUpdateCheck(now: () => number = Date.now): void {
  const settings = useSettings();
  const toast = useToast();
  const t = useT(updateStrings);
  const qc = useQueryClient();
  const started = useRef(false);
  const latest = useRef({ toast, t, now });
  latest.current = { toast, t, now };
  const enabled = settings.data?.["updates.autoCheck"];

  useEffect(() => {
    if (enabled !== true || started.current) return;
    started.current = true;
    if (!autoCheckDue(latest.current.now(), readLastAutoCheck())) return;
    void (async () => {
      try {
        const token = await unwrap(ipc().commands.updaterTokenStatus());
        if (token === "missing") return;
        writeLastAutoCheck(latest.current.now());
        const result = await unwrap(ipc().commands.updateCheck());
        if (result.kind === "available") {
          const { toast: show, t: tr } = latest.current;
          show.show(tr("backgroundAvailable", { version: result.version }), NOTICE_MS);
        }
      } catch {
        // Offline or refused: not news at start-up. Settings shows a refused token.
      } finally {
        void qc.invalidateQueries({ queryKey: queryKeys.updaterToken });
      }
    })();
  }, [enabled, qc]);
}
