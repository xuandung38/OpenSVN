import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
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
  /** List the revisions each entry merged in (`svn log -g`), grouped under it. */
  showMerged: boolean;
}

export const NO_FILTERS: HistoryFilters = {
  text: "",
  author: "",
  from: "",
  to: "",
  stopOnCopy: false,
  showMerged: false,
};

/** One row of the list: an entry of the node's history, or a revision a merge brought in. */
export interface HistoryRow {
  entry: LogEntry;
  /** 0 for the node's own history, 1+ for merged-in revisions (nested merges go deeper). */
  depth: number;
  /** The node's own revision that merged this one in (`null` at depth 0). */
  mergedBy: number | null;
}

/** Rows of `entries` with their merged revisions right below each, depth first. */
export function flattenMerged(entries: readonly LogEntry[]): HistoryRow[] {
  const out: HistoryRow[] = [];
  const walk = (list: readonly LogEntry[], depth: number, mergedBy: number | null) => {
    for (const e of list) {
      out.push({ entry: e, depth, mergedBy });
      if (e.merged?.length) walk(e.merged, depth + 1, depth === 0 ? e.rev : mergedBy);
    }
  };
  walk(entries, 0, null);
  return out;
}

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
      ? [...queryKeys.urlLog(target.url, text, filters.stopOnCopy), filters.showMerged]
      : [
          ...queryKeys.wcLog(target.wcId, target.path, text, filters.stopOnCopy),
          filters.showMerged,
        ];
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
          // The list shows no changed paths: they come per revision (`useRevisionPaths`), so
          // a page never pays for a revision that touched thousands of paths.
          withPaths: false,
          filter: text || null,
          includeMerged: filters.showMerged,
        }),
      ),
    getNextPageParam: (last) => last.nextRev ?? undefined,
  });
  // Paging contract: `nextRev` is the first revision not yet returned; the next request sends it as
  // `fromRev` (inclusive, newest first). Dedupe by `rev` anyway so an off-by-one on the backend can
  // never show a revision twice.
  const all = useMemo(() => {
    const seen = new Set<number>();
    const out: LogEntry[] = [];
    for (const page of q.data?.pages ?? []) {
      for (const e of page.entries) {
        if (!seen.has(e.rev)) {
          seen.add(e.rev);
          out.push(e);
        }
      }
    }
    return out;
  }, [q.data]);
  const authors = useMemo(
    () => [...new Set(all.map((e) => e.author).filter((a): a is string => !!a))].sort(),
    [all],
  );
  const entries = useMemo(() => applyClientFilters(all, filters), [all, filters]);
  const rows = useMemo(() => flattenMerged(entries), [entries]);
  return { ...q, entries, rows, authors, loaded: all.length };
}

/**
 * Changed paths of revisions `from..=to` (newest first) of `target`, read when shown: the list
 * itself is read without them. A merged-in revision is not in `target`'s own history, so it is
 * read from the repository root.
 */
export function useRevisionPaths(
  target: LogTarget,
  range: { from: number; to: number } | null,
  repoRootUrl: string,
  merged = false,
) {
  const scope: LogTarget = merged ? { kind: "url", url: repoRootUrl } : target;
  return useQuery({
    queryKey: [...queryKeys.logRoot, "paths", scope, range?.from ?? 0, range?.to ?? 0],
    enabled: range !== null,
    staleTime: Number.POSITIVE_INFINITY,
    queryFn: async () => {
      const r = range as { from: number; to: number };
      const page = await unwrap(
        ipc().commands.logGet({
          target: scope,
          fromRev: r.to,
          toRev: r.from,
          limit: Math.min(1000, r.to - r.from + 1),
          stopOnCopy: null,
          withPaths: true,
          filter: null,
        }),
      );
      return page.entries;
    },
  });
}
