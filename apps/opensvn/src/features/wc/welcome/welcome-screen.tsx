import { useQueryClient } from "@tanstack/react-query";
import { useCallback } from "react";
import { Button } from "../../../components/ui/button";
import { EmptyState } from "../../../components/ui/empty-state";
import { useToast } from "../../../components/ui/toast";
import type { PathGrantInfo } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { dialogStrings } from "../dialogs/dialog-strings";
import { shortcutLabel } from "../shortcuts";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { addExistingWorkingCopy, useDroppedFolders } from "./use-path-grant";

/** Registers a working copy through the grant flow, toasting the outcome. Shared with the menus. */
export function useAddExistingWorkingCopy() {
  const t = useT(wcStrings);
  const d = useT(dialogStrings);
  const toast = useToast();
  const qc = useQueryClient();
  return useCallback(async () => {
    try {
      const wc = await addExistingWorkingCopy(d("destination"));
      if (!wc) return;
      await qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
      toast.show(t("added", { name: wc.name }));
    } catch (e) {
      toast.show(toAppError(e).message, 6000);
    }
  }, [d, qc, t, toast]);
}

/**
 * Folders dropped on the window are added as working copies (`fs://dropped` carries grants, so no
 * raw path reaches the WebView).
 */
export function DropToAddWorkingCopy() {
  const t = useT(wcStrings);
  const toast = useToast();
  const qc = useQueryClient();
  const onFolder = useCallback(
    (g: PathGrantInfo) => {
      unwrap(ipc().commands.wcAddExisting(g.id))
        .then((wc) => {
          void qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
          toast.show(t("added", { name: wc.name }));
        })
        .catch((e: unknown) => toast.show(toAppError(e).message, 6000));
    },
    [qc, t, toast],
  );
  const onIgnored = useCallback(() => toast.show(t("dropIgnored")), [t, toast]);
  useDroppedFolders(onFolder, onIgnored);
  return null;
}

/** Shown when no working copy is registered yet (doc 01 section 2, Welcome). */
export function WelcomeScreen() {
  const t = useT(wcStrings);
  const { openDialog } = useWcSession();
  const addExisting = useAddExistingWorkingCopy();
  return (
    <EmptyState
      title={t("welcomeTitle")}
      description={`${t("welcomeHint")} ${t("dropHint")}`}
      actions={
        <>
          <Button
            variant="primary"
            title={shortcutLabel("checkout")}
            onClick={() => openDialog({ kind: "checkout" })}
          >
            {t("checkoutAction")}
          </Button>
          <Button title={shortcutLabel("addExisting")} onClick={() => void addExisting()}>
            {t("addExistingAction")}
          </Button>
        </>
      }
    />
  );
}
