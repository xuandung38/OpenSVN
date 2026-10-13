import type { NodeStatus } from "../ipc/bindings";

/** One-letter SVN status glyphs (design-guidelines 7). */
export type StatusCode = "M" | "A" | "D" | "R" | "?" | "!" | "C" | "P";

const NODE_STATUS_TO_CODE: Partial<Record<NodeStatus, StatusCode>> = {
  modified: "M",
  added: "A",
  deleted: "D",
  replaced: "R",
  unversioned: "?",
  missing: "!",
  conflicted: "C",
};

/** Map a backend `NodeStatus` to its glyph; `null` for states with no glyph (normal, ignored...). */
export function statusCode(status: NodeStatus): StatusCode | null {
  return NODE_STATUS_TO_CODE[status] ?? null;
}

/** Human-readable label, used as tooltip and accessible name (never colour-only). */
export const STATUS_LABEL: Record<StatusCode, string> = {
  M: "Modified",
  A: "Added",
  D: "Deleted",
  R: "Replaced / moved",
  "?": "Unversioned",
  "!": "Missing",
  C: "Conflicted",
  P: "Properties changed",
};
