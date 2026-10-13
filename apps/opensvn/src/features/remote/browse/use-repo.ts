import { useQuery } from "@tanstack/react-query";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";

/** Directory listing at `rev` (null = HEAD). Disabled until `enabled`. */
export function useRepoList(url: string | null, rev: number | null, enabled = true) {
  return useQuery({
    queryKey: queryKeys.repoList(url ?? "", rev),
    enabled: enabled && url !== null,
    queryFn: () => unwrap(ipc().commands.repoList(url as string, rev)),
  });
}

/** File content for the Preview tab (the backend refuses oversized files with an AppError). */
export function useRepoFile(url: string | null, rev: number | null) {
  return useQuery({
    queryKey: queryKeys.repoFile(url ?? "", rev),
    enabled: url !== null,
    queryFn: () => unwrap(ipc().commands.repoCat(url as string, rev)),
  });
}

export function useRepoInfo(url: string | null) {
  return useQuery({
    queryKey: queryKeys.repoInfo(url ?? ""),
    enabled: url !== null,
    queryFn: () => unwrap(ipc().commands.repoInfo(url as string)),
  });
}
