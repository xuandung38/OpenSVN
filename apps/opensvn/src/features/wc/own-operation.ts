import type { OperationKind, WorkingCopy } from "../../ipc/bindings";
import type { OpState } from "../../ipc/events";

/** Kinds that never write (or lock) a working copy; mirrors Rust `ops::manager::writes_wc`. */
const READ_ONLY_KINDS: ReadonlySet<OperationKind> = new Set([
  "status-remote",
  "status-refresh",
  "log",
  "blame",
  "remote-edit",
  "copy",
  "save",
]);

/** Path without trailing separators; Windows paths compare case-insensitively (NTFS). */
function normalize(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  return /^[a-zA-Z]:[\\/]|\\/.test(trimmed) ? trimmed.replaceAll("\\", "/").toLowerCase() : trimmed;
}

/** Whether `a` and `b` are the same folder or one contains the other. */
export function relatedPaths(a: string, b: string): boolean {
  const x = normalize(a);
  const y = normalize(b);
  if (x === y) return true;
  return x.startsWith(`${y}/`) || y.startsWith(`${x}/`);
}

/**
 * The running operation of this app that writes `wc` (by id, or by folder for a checkout that is
 * still creating it). Its lock on the working copy is ours, not a leftover of a crash.
 */
export function ownOperationOn(
  ops: readonly OpState[],
  wc: Pick<WorkingCopy, "id" | "rootPath">,
): OpState | null {
  return (
    ops.find(
      (o) =>
        o.state === "running" &&
        !o.handle.background &&
        !READ_ONLY_KINDS.has(o.handle.kind) &&
        (o.handle.wcId === wc.id ||
          (o.handle.rootPath != null && relatedPaths(o.handle.rootPath, wc.rootPath))),
    ) ?? null
  );
}

/** What the lock banner shows. */
export type LockBanner =
  /** One of our operations holds the working copy: say so, never offer cleanup. */
  | { kind: "in-progress"; op: OpState }
  /** A lock no operation of ours explains: offer cleanup. */
  | { kind: "stale" }
  | null;

/**
 * Lock banner of `wc`: the stale-lock banner ("Run cleanup") only when no operation of ours
 * holds the working copy; while one does (a checkout still filling it), an "in progress" note.
 */
export function lockBanner(
  ops: readonly OpState[],
  wc: Pick<WorkingCopy, "id" | "rootPath">,
  needsCleanup: boolean,
): LockBanner {
  const own = ownOperationOn(ops, wc);
  if (own && (needsCleanup || own.handle.kind === "checkout"))
    return { kind: "in-progress", op: own };
  if (own) return null;
  return needsCleanup ? { kind: "stale" } : null;
}
