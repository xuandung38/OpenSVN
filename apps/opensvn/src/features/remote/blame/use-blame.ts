import { useQuery } from "@tanstack/react-query";
import type { AppError, BlameLine } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { useOperationOutcome } from "../../../ipc/queries";

export interface BlameState {
  status: "loading" | "ready" | "error";
  lines: BlameLine[];
  error: AppError | null;
}

/**
 * Starts `blame_get` (returns an operation handle) and reads the result from the operation outcome
 * delivered by `op://finished`/`op://failed`. The outcome is cached by op id, so a result that
 * arrives before the handle is known is not lost.
 */
export function useBlame(url: string | null, rev: number | null): BlameState {
  const start = useQuery({
    queryKey: ["blame-start", url, rev],
    enabled: url !== null,
    staleTime: Number.POSITIVE_INFINITY,
    queryFn: () => unwrap(ipc().commands.blameGet(url as string, rev)),
  });
  const outcome = useOperationOutcome(start.data?.opId);
  if (start.error) {
    const e = start.error as { appError?: AppError; message: string };
    return { status: "error", lines: [], error: e.appError ?? null };
  }
  if (outcome?.status === "failed") return { status: "error", lines: [], error: outcome.error };
  if (outcome?.status === "ok" && outcome.result.kind === "blame") {
    return { status: "ready", lines: outcome.result.lines, error: null };
  }
  return { status: "loading", lines: [], error: null };
}
