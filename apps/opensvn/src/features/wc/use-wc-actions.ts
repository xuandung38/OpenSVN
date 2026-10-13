import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useMemo } from "react";
import { useToast } from "../../components/ui/toast";
import type { OpenWith, Rev } from "../../ipc/bindings";
import { ipc, toAppError, unwrap } from "../../ipc/client";
import { queryKeys } from "../../ipc/query-keys";
import { useT } from "../../lib/i18n/i18n";
import { useWcSession } from "./wc-session";
import { wcStrings } from "./wc-strings";

/**
 * Commands of the current working copy, each reporting failures as a toast with the backend's
 * message. Commands that run as operations (update, add, delete, ...) are tracked in the Activity
 * panel through `op://*` events; the instant ones (changelist, ignore, resolve) refresh the status.
 */
export function useWcActions() {
  const { wc } = useWcSession();
  const toast = useToast();
  const qc = useQueryClient();
  const t = useT(wcStrings);
  const wcId = wc?.id ?? null;

  /** Runs a call; resolves to `undefined` after showing the error. */
  const attempt = useCallback(
    async <T>(call: () => Promise<T>): Promise<T | undefined> => {
      try {
        return await call();
      } catch (e) {
        toast.show(toAppError(e).message, 6000);
        return undefined;
      }
    },
    [toast],
  );

  const refreshStatus = useCallback(() => {
    if (wcId) void qc.invalidateQueries({ queryKey: queryKeys.status(wcId) });
  }, [qc, wcId]);

  return useMemo(
    () => ({
      attempt,
      refreshStatus,
      async update(rev: Rev | null = null) {
        if (!wcId) return;
        const h = await attempt(() =>
          unwrap(ipc().commands.update({ wcId, paths: null, rev, depth: null })),
        );
        if (h) toast.show(t("updateRequested"));
      },
      async checkForUpdates() {
        if (!wcId) return;
        const h = await attempt(() => unwrap(ipc().commands.statusRemote(wcId)));
        if (h) toast.show(t("checkRequested"));
      },
      async add(paths: string[]) {
        if (!wcId) return;
        await attempt(() => unwrap(ipc().commands.addPaths(wcId, paths)));
      },
      async recordMove(from: string, to: string) {
        if (!wcId) return;
        await attempt(() => unwrap(ipc().commands.markMove(wcId, from, to)));
        refreshStatus();
      },
      async reveal(path: string) {
        if (!wcId) return;
        await attempt(() => unwrap(ipc().commands.revealInOs(wcId, path)));
      },
      async openWith(path: string, how: OpenWith) {
        if (!wcId) return;
        await attempt(() => unwrap(ipc().commands.openExternal(wcId, path, how)));
      },
      /** Opens the file's base-vs-working diff in the configured external diff tool. */
      async openExternalDiff(path: string) {
        if (!wcId) return;
        await attempt(() => unwrap(ipc().commands.diffExternal({ kind: "wc", wcId, path })));
      },
      async copyPath(path: string, relative: boolean) {
        const text = relative || !wc ? path : `${wc.rootPath}/${path}`;
        try {
          await navigator.clipboard.writeText(text);
          toast.show(t("copied"));
        } catch (e) {
          toast.show(toAppError(e).message);
        }
      },
      async cancelOp(opId: string) {
        const ok = await attempt(() => unwrap(ipc().commands.opCancel(opId)));
        if (ok !== undefined) toast.show(t("cancelRequested"));
      },
    }),
    [attempt, refreshStatus, wcId, wc, toast, t],
  );
}

export type WcActions = ReturnType<typeof useWcActions>;
