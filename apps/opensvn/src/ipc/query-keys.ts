import type { WcId } from "./bindings";

/** Central query-key factory so event handlers and hooks invalidate the same keys. */
export const queryKeys = {
  workingCopies: ["wcs"] as const,
  status: (wcId: WcId) => ["status", wcId] as const,
  logRoot: ["log"] as const,
  log: (wcId: WcId, path: string, limit: number) => ["log", wcId, path, limit] as const,
  banners: (wcId: WcId) => ["banners", wcId] as const,
  ops: ["ops"] as const,
  settings: ["settings"] as const,
};
