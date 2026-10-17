import { type QueryClient, replaceEqualDeep } from "@tanstack/react-query";
import type { FileStatus, StatusChanged, StatusSnapshot } from "./bindings";
import { queryKeys } from "./query-keys";

/** Order of the backend's lists: paths by Unicode code point (UTF-8 byte order), not UTF-16. */
export function comparePaths(a: string, b: string): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    const x = a.codePointAt(i) ?? 0;
    const y = b.codePointAt(i) ?? 0;
    if (x !== y) return x < y ? -1 : 1;
    if (x > 0xffff) i++;
  }
  return a.length - b.length;
}

/** What a holder of `held` does with `delta`: apply it, ignore it (already has it) or refetch. */
export function deltaAction(
  held: number | null,
  delta: Pick<StatusChanged, "base" | "snapshot">,
): "apply" | "ignore" | "refetch" {
  if (held === null) return "refetch";
  if (delta.base !== null && held === delta.base) return "apply";
  // Ids grow over the app's lifetime: a holder of this snapshot or a later one has the change.
  if (held >= delta.snapshot) return "ignore";
  return "refetch";
}

/** `files` with `delta` applied, sorted like the backend's lists. */
export function applyDelta(
  files: FileStatus[],
  delta: Pick<StatusChanged, "changed" | "removed">,
): FileStatus[] {
  const byPath = new Map(files.map((f) => [f.path, f]));
  for (const p of delta.removed) byPath.delete(p);
  for (const f of delta.changed) byPath.set(f.path, f);
  return [...byPath.values()].sort((a, b) => comparePaths(a.path, b.path));
}

/** True for the status list keys (`useStatus`), as opposed to diffs and previews below them. */
function isListKey(key: readonly unknown[]): boolean {
  const last = key[2];
  return key.length === 3 && typeof last === "object" && last !== null && "includeIgnored" in last;
}

/**
 * `wc://status-changed`: lists holding the delta's base snapshot take the delta, lists that
 * already hold it stay, the others (and the diffs/previews under the status key) refetch.
 */
export function applyStatusChanged(qc: QueryClient, p: StatusChanged): void {
  const prefix = queryKeys.status(p.wcId);
  for (const q of qc.getQueryCache().findAll({ queryKey: prefix })) {
    const key = q.queryKey;
    const data = q.state.data as StatusSnapshot | undefined;
    const action = isListKey(key) && data ? deltaAction(data.snapshot, p) : "refetch";
    if (action === "apply" && data) {
      qc.setQueryData<StatusSnapshot>(key, {
        snapshot: p.snapshot,
        files: applyDelta(data.files, p),
      });
    } else if (action === "refetch") {
      void qc.invalidateQueries({ queryKey: key, exact: true });
    }
  }
}

/**
 * Keeps the later of two snapshots when a fetch that started before a delta was applied returns
 * after it (as `structuralSharing` of the status query).
 */
export function keepLaterSnapshot(oldData: unknown, newData: unknown): unknown {
  const o = oldData as StatusSnapshot | undefined;
  const n = newData as StatusSnapshot;
  if (o && o.snapshot !== null && n.snapshot !== null && o.snapshot > n.snapshot) return o;
  return replaceEqualDeep(o, n);
}
