import type { WcId } from "./bindings";

/** Central query-key factory so event handlers and hooks invalidate the same keys. */
export const queryKeys = {
  workingCopies: ["wcs"] as const,
  /** Under `workingCopies`, so invalidating the list also refreshes the repository manager. */
  wcEntries: ["wcs", "entries"] as const,
  recentUrls: ["recent-urls"] as const,
  status: (wcId: WcId) => ["status", wcId] as const,
  logRoot: ["log"] as const,
  log: (wcId: WcId, path: string, limit: number) => ["log", wcId, path, limit] as const,
  banners: (wcId: WcId) => ["banners", wcId] as const,
  repoRoot: ["repo"] as const,
  repoList: (url: string, rev: number | null) => ["repo", "list", url, rev] as const,
  repoFile: (url: string, rev: number | null) => ["repo", "file", url, rev] as const,
  repoInfo: (url: string) => ["repo", "info", url] as const,
  urlLog: (url: string, filter: string, stopOnCopy: boolean) =>
    ["log", "url", url, filter, stopOnCopy] as const,
  wcLog: (wcId: string, path: string, filter: string, stopOnCopy: boolean) =>
    ["log", "wc", wcId, path, filter, stopOnCopy] as const,
  layout: (repoUuid: string) => ["layout", repoUuid] as const,
  branches: (repoRootUrl: string) => ["branches", repoRootUrl] as const,
  accounts: ["accounts"] as const,
  ops: ["ops"] as const,
  opOutcomes: ["op-outcomes"] as const,
  settings: ["settings"] as const,
  engineCapabilities: ["engine-capabilities"] as const,
};
