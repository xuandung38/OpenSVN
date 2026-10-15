import type { ActionCounts, OpResult, OpStage, PathAction } from "../../ipc/bindings";
import type { LogLine, OpState } from "../../ipc/events";
import type { ActivityKey } from "./activity-strings";

/** The first column `svn` prints for each action. */
export const ACTION_LETTER: Record<PathAction, string> = {
  added: "A",
  deleted: "D",
  updated: "U",
  merged: "G",
  conflicted: "C",
  "tree-conflicted": "C",
  existed: "E",
  restored: "R",
  skipped: "S",
};

/** One log line as `svn` prints it: `A    src/lib.rs`. The root shows as `.`. */
export function formatLogLine(line: LogLine): string {
  return `${ACTION_LETTER[line.action]}    ${line.path || "."}`;
}

/** Summary rows: label key and count, in `svn`'s order, zero counts left out. */
export function countRows(counts: ActionCounts | null): [ActivityKey, number][] {
  if (!counts) return [];
  const rows: [ActivityKey, number][] = [
    ["countAdded", counts.added],
    ["countUpdated", counts.updated],
    ["countDeleted", counts.deleted],
    ["countMerged", counts.merged],
    ["countConflicted", counts.conflicted],
    ["countTreeConflicted", counts.treeConflicted],
    ["countExisted", counts.existed],
    ["countRestored", counts.restored],
    ["countSkipped", counts.skipped],
  ];
  return rows.filter(([, n]) => n > 0);
}

/** The revision a finished operation reached, if it reports one. */
export function finalRevision(result: OpResult | null): number | null {
  if (!result) return null;
  if (result.kind === "newRev" || result.kind === "checkedOut") return result.newRev;
  return null;
}

/** Label key of the operation's current stage. */
export function stageKey(op: OpState): ActivityKey {
  if (op.state === "done") return "stageDone";
  if (op.state === "failed") return "stageFailed";
  const stage: OpStage | null | undefined = op.progress?.stage;
  switch (stage) {
    case "connecting":
      return "stageConnecting";
    case "fetching":
      return "stageFetching";
    case "finishing":
      return "stageFinishing";
    case "waiting-for-connection":
      return "stageWaiting";
    default:
      return "stageWorking";
  }
}

/** Elapsed time as `m:ss` (or `h:mm:ss`). */
export function formatElapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const ss = String(s).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

/** Plain-text log for the clipboard: the paths, then the summary counts and warnings. */
export function logText(op: OpState, label: (key: ActivityKey) => string): string {
  const lines = op.log.map(formatLogLine);
  const counts = countRows(op.progress?.counts ?? null).map(([k, n]) => `${label(k)}: ${n}`);
  const rev = finalRevision(op.result);
  return [
    op.handle.title,
    ...lines,
    ...(counts.length ? ["", ...counts] : []),
    ...(op.warnings.length ? ["", `${label("warnings")}:`, ...op.warnings] : []),
    ...(rev !== null ? ["", `r${rev}`] : []),
    ...(op.appError
      ? ["", op.appError.message, ...(op.appError.detail ? [op.appError.detail] : [])]
      : []),
  ].join("\n");
}
