import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { Banner, CommitRequest, UpdateRequest, WcId } from "./bindings";
import { ipc, unwrap } from "./client";
import type { OpOutcome, OpState } from "./events";
import { queryKeys } from "./query-keys";

export function useWorkingCopies() {
  return useQuery({
    queryKey: queryKeys.workingCopies,
    queryFn: () => unwrap(ipc().commands.wcList()),
  });
}

export function useStatus(wcId: WcId | null) {
  return useQuery({
    queryKey: queryKeys.status(wcId ?? ""),
    enabled: wcId !== null,
    queryFn: () =>
      unwrap(
        ipc().commands.statusGet({
          wcId: wcId as WcId,
          includeIgnored: false,
          includeUnversioned: true,
        }),
      ),
  });
}

export function useLog(wcId: WcId | null, path = "", limit = 100) {
  return useQuery({
    queryKey: queryKeys.log(wcId ?? "", path, limit),
    enabled: wcId !== null,
    queryFn: () =>
      unwrap(
        ipc().commands.logGet({
          target: { kind: "wc", wcId: wcId as WcId, path },
          fromRev: null,
          toRev: null,
          limit,
          stopOnCopy: null,
          withPaths: false,
          filter: null,
        }),
      ),
  });
}

export function useSettings() {
  return useQuery({
    queryKey: queryKeys.settings,
    queryFn: () => unwrap(ipc().commands.settingsGet()),
  });
}

/** Banners are pushed by `wc://banner`; the cache starts empty. */
export function useBanners(wcId: WcId | null) {
  return useQuery<Banner[]>({
    queryKey: queryKeys.banners(wcId ?? ""),
    enabled: false,
    initialData: [],
    queryFn: () => [],
  });
}

/** Operations map maintained by the event bus (op://*), newest last. */
export function useOperations(): OpState[] {
  const { data } = useQuery<Record<string, OpState>>({
    queryKey: queryKeys.ops,
    enabled: false,
    initialData: {},
    queryFn: () => ({}),
  });
  return Object.values(data);
}

/** Starts an update; progress arrives through op://* events, status through wc://* events. */
export function useUpdate() {
  return useMutation({ mutationFn: (req: UpdateRequest) => unwrap(ipc().commands.update(req)) });
}

export function useCommit() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: CommitRequest) => unwrap(ipc().commands.commit(req)),
    onSuccess: (_h, req) => qc.invalidateQueries({ queryKey: queryKeys.status(req.wcId) }),
  });
}

/** Final outcome of one operation (result or error), `undefined` while it is still running. */
export function useOperationOutcome(opId: string | undefined): OpOutcome | undefined {
  const { data } = useQuery<Record<string, OpOutcome>>({
    queryKey: queryKeys.opOutcomes,
    enabled: false,
    initialData: {},
    queryFn: () => ({}),
  });
  return opId ? data[opId] : undefined;
}
