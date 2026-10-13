import { useInfiniteQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import type { LogEntry, LogTarget } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";

export const PAGE_SIZE = 100;

export interface HistoryFilters {
  /** Server-side filter over message/author/path. */
  text: string;
  /** Client-side filters over the loaded pages. */
  author: string;
  /** `YYYY-MM-DD` (UTC), inclusive; empty = no bound. */
  from: string;
  to: string;
  stopOnCopy: boolean;
}

export const NO_FILTERS: HistoryFilters = {
  text: "",
  author: "",
  from: "",
  to: "",
  stopOnCopy: false,
};

/** Applies the client-side author and date filters (UTC calendar days, inclusive). */
export function applyClientFilters(entries: LogEntry[], f: HistoryFilters): LogEntry[] {
  return entries.filter((e) => {
    if (f.author && (e.author ?? "") !== f.author) return false;
    const day = e.date.slice(0, 10);
    if (f.from && day < f.from) return false;
    if (f.to && day > f.to) return false;
    return true;
  });
}

/** Paged log (100 per page, newest first); `nextRev` from the backend drives the next page. */
export function useHistory(target: LogTarget, filters: HistoryFilters) {
  const text = filters.text.trim();
  const key =
    target.kind === "url"
      ? queryKeys.urlLog(target.url, text, filters.stopOnCopy)
      : queryKeys.wcLog(target.wcId, target.path, text, filters.stopOnCopy);
  const q = useInfiniteQuery({
    queryKey: key,
    initialPageParam: null as number | null,
    queryFn: ({ pageParam }) =>
      unwrap(
        ipc().commands.logGet({
          target,
          fromRev: pageParam,
          toRev: null,
          limit: PAGE_SIZE,
          stopOnCopy: filters.stopOnCopy,
          withPaths: true,
          filter: text || null,
        }),
      ),
    getNextPageParam: (last) => last.nextRev ?? undefined,
  });
  const all = useMemo(() => q.data?.pages.flatMap((p) => p.entries) ?? [], [q.data]);
  const authors = useMemo(
    () => [...new Set(all.map((e) => e.author).filter((a): a is string => !!a))].sort(),
    [all],
  );
  const entries = useMemo(() => applyClientFilters(all, filters), [all, filters]);
  return { ...q, entries, authors, loaded: all.length };
}
