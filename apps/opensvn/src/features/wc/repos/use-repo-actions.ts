import { useQueryClient } from "@tanstack/react-query";
import { useMemo } from "react";
import { useToast } from "../../../components/ui/toast";
import type { WcEntry } from "../../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { useT } from "../../../lib/i18n/i18n";
import { pickFolder } from "../welcome/use-path-grant";
import { repoStrings } from "./repo-strings";

export interface RepoActions {
  /** Opens the working copy (recovery + status refresh) and makes it current. */
  switchTo: (entry: Pick<WcEntry, "id" | "name">) => Promise<void>;
  reveal: (entry: WcEntry) => Promise<void>;
  copyUrl: (entry: WcEntry) => Promise<void>;
  /** Re-grants a moved working copy through the native folder dialog. */
  locate: (entry: WcEntry) => Promise<void>;
}

/**
 * Actions of the repository manager. Errors become toasts; the list is refreshed afterwards because
 * a failure often means the folder went missing.
 */
export function useRepoActions(onSelect: (wcId: string) => void): RepoActions {
  const r = useT(repoStrings);
  const toast = useToast();
  const qc = useQueryClient();
  return useMemo<RepoActions>(() => {
    const refresh = () => qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
    return {
      switchTo: async (entry) => {
        try {
          await unwrap(ipc().commands.wcOpen(entry.id));
          onSelect(entry.id);
        } catch (e) {
          toast.show(r("switchFailed", { name: entry.name, message: toAppError(e).message }), 6000);
        }
        await refresh();
      },
      reveal: async (entry) => {
        try {
          await unwrap(ipc().commands.revealInOs(entry.id, ""));
        } catch (e) {
          toast.show(toAppError(e).message, 6000);
          await refresh();
        }
      },
      copyUrl: async (entry) => {
        try {
          await navigator.clipboard.writeText(entry.url ?? entry.repoRootUrl);
          toast.show(r("urlCopied"));
        } catch {
          toast.show(r("copyFailed"));
        }
      },
      locate: async (entry) => {
        try {
          const grant = await pickFolder(r("locateTitle", { name: entry.name }));
          if (!grant) return;
          await unwrap(ipc().commands.wcLocate(entry.id, grant.id));
          toast.show(r("located", { name: entry.name }));
        } catch (e) {
          toast.show(toAppError(e).message, 6000);
        }
        await refresh();
      },
    };
  }, [onSelect, qc, r, toast]);
}
