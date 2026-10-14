import type { FileStatus } from "../../ipc/bindings";
import { type StatusCode, statusCode } from "../../lib/status";

/** Status glyph of a changed file, or `null` when the row is not a local change (normal, ignored...). */
export function fileGlyph(f: FileStatus): StatusCode | null {
  if (isConflicted(f)) return "C";
  const code = statusCode(f.text);
  if (code) return code;
  if (f.props === "modified") return "P";
  return null;
}

export function isConflicted(f: FileStatus): boolean {
  return f.treeConflict || f.text === "conflicted" || f.props === "conflicted";
}

/** Files shown in Changes: local modifications, plus unversioned/missing entries. */
export function changedFiles(files: readonly FileStatus[]): FileStatus[] {
  return files.filter((f) => fileGlyph(f) !== null);
}

/**
 * Unmodified files that matter for locking (doc 01 §10.4): read-only `svn:needs-lock` files to lock
 * before editing, and locked files to unlock. They are not changes, so never part of a commit.
 */
export function lockFiles(files: readonly FileStatus[]): FileStatus[] {
  return files.filter(
    (f) => f.kind === "file" && fileGlyph(f) === null && (f.needsLock || f.lock !== null),
  );
}

/**
 * Pre-ticked on first sight: tracked changes. Unversioned (`?`) files are never auto-ticked (a tick
 * means "add on commit"), and conflicted files cannot be committed.
 */
export function tickedByDefault(f: FileStatus): boolean {
  return f.text !== "unversioned" && !isConflicted(f);
}

export type ChipId = "modified" | "added" | "unversioned" | "conflicted" | "locked";
export const CHIPS: readonly ChipId[] = [
  "modified",
  "added",
  "unversioned",
  "conflicted",
  "locked",
];

const CHIP_TEST: Record<ChipId, (f: FileStatus) => boolean> = {
  modified: (f) => f.text === "modified" || f.props === "modified",
  added: (f) => f.text === "added" || f.text === "replaced",
  unversioned: (f) => f.text === "unversioned",
  conflicted: isConflicted,
  locked: (f) => f.lock !== null,
};

/** Text filter plus quick-filter chips (OR between chips, AND with the text). */
export function filterFiles(
  files: readonly FileStatus[],
  text: string,
  chips: ReadonlySet<ChipId>,
): FileStatus[] {
  const q = text.trim().toLowerCase();
  return files.filter(
    (f) =>
      (!q || f.path.toLowerCase().includes(q)) &&
      (chips.size === 0 || [...chips].some((c) => CHIP_TEST[c](f))),
  );
}

export type ListRow =
  | { kind: "group"; name: string | null; paths: string[] }
  | { kind: "file"; file: FileStatus };

/**
 * Flat rows for the virtual list. Group headers appear only when at least one file belongs to a
 * changelist; files without changelist come first (doc 01 3.3).
 */
export function buildRows(files: readonly FileStatus[]): ListRow[] {
  if (!files.some((f) => f.changelist !== null)) {
    return files.map((file) => ({ kind: "file", file }));
  }
  const groups = new Map<string | null, FileStatus[]>();
  for (const f of files) {
    const list = groups.get(f.changelist) ?? [];
    list.push(f);
    groups.set(f.changelist, list);
  }
  const names = [...groups.keys()].sort((a, b) =>
    a === null ? -1 : b === null ? 1 : a.localeCompare(b),
  );
  const rows: ListRow[] = [];
  for (const name of names) {
    const list = groups.get(name) ?? [];
    rows.push({ kind: "group", name, paths: list.map((f) => f.path) });
    for (const file of list) rows.push({ kind: "file", file });
  }
  return rows;
}

export type TriState = "none" | "some" | "all";

export function tristate(paths: readonly string[], checked: ReadonlySet<string>): TriState {
  const n = paths.filter((p) => checked.has(p)).length;
  return n === 0 ? "none" : n === paths.length ? "all" : "some";
}

/**
 * Text shown for a status path. The working copy root has the empty path (a property change on
 * the root, e.g. `svn:ignore`); it is shown as `rootLabel` instead of an empty row.
 */
export function displayPath(path: string, rootLabel: string): string {
  return path === "" ? rootLabel : path;
}

export function baseName(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1);
}

export function parentDir(path: string): string {
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i);
}

/** `missing` + `unversioned` pair with the same base name in different folders: likely a move. */
export function suggestMoves(files: readonly FileStatus[]): { from: string; to: string }[] {
  const unversioned = new Map<string, string>();
  for (const f of files) if (f.text === "unversioned") unversioned.set(baseName(f.path), f.path);
  const out: { from: string; to: string }[] = [];
  for (const f of files) {
    const to = f.text === "missing" ? unversioned.get(baseName(f.path)) : undefined;
    if (to && to !== f.path) out.push({ from: f.path, to });
  }
  return out;
}

export interface CommitCheck {
  /** Selected files that block the commit. */
  conflicts: string[];
  /** Selected files that need a lock the user does not hold. */
  needsLock: string[];
  /** Selected files locked by someone else (the server would refuse them). */
  lockedByOther: string[];
  /** Selected files that will be added on commit. */
  willAdd: number;
}

export function checkCommit(selected: readonly FileStatus[]): CommitCheck {
  return {
    conflicts: selected.filter(isConflicted).map((f) => f.path),
    needsLock: selected.filter((f) => f.needsLock && !f.lock?.mine).map((f) => f.path),
    lockedByOther: selected.filter((f) => f.lock && !f.lock.mine).map((f) => f.path),
    willAdd: selected.filter((f) => f.text === "unversioned").length,
  };
}

export const SUMMARY_SOFT_LIMIT = 72;

export function joinMessage(summary: string, description: string): string {
  const s = summary.trim();
  const d = description.trim();
  return d ? `${s}\n\n${d}` : s;
}

/** Splits a stored message back into the two commit-box fields. */
export function splitMessage(message: string): { summary: string; description: string } {
  const i = message.indexOf("\n");
  if (i < 0) return { summary: message, description: "" };
  return { summary: message.slice(0, i), description: message.slice(i + 1).replace(/^\n+/, "") };
}

const HISTORY_KEY = "opensvn.commitMessages";
export const HISTORY_MAX = 20;

function safeStorage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

/** Recent commit messages, newest first. Corrupt storage yields an empty list. */
export function loadMessageHistory(
  storage: Pick<Storage, "getItem"> | null = safeStorage(),
): string[] {
  try {
    const raw = storage?.getItem(HISTORY_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed)
      ? parsed
          .filter((m): m is string => typeof m === "string" && m.length > 0)
          .slice(0, HISTORY_MAX)
      : [];
  } catch {
    return [];
  }
}

export function pushMessageHistory(
  message: string,
  storage: Pick<Storage, "getItem" | "setItem"> | null = safeStorage(),
): string[] {
  const next = [message, ...loadMessageHistory(storage).filter((m) => m !== message)].slice(
    0,
    HISTORY_MAX,
  );
  try {
    storage?.setItem(HISTORY_KEY, JSON.stringify(next));
  } catch {
    // Storage full or blocked: history is a convenience, never fail a commit over it.
  }
  return next;
}

/** `*.ext` pattern for a path, or `null` when the file has no extension. */
export function extensionPattern(path: string): string | null {
  const name = baseName(path);
  const dot = name.lastIndexOf(".");
  return dot > 0 && dot < name.length - 1 ? `*${name.slice(dot)}` : null;
}

/** Repository URL of a working-copy-relative path (each segment percent-encoded). */
export function wcPathUrl(wcUrl: string, path: string): string {
  return path
    .split("/")
    .filter(Boolean)
    .reduce((url, seg) => `${url}/${encodeURIComponent(seg)}`, wcUrl.replace(/\/+$/, ""));
}

/** Accepts only schemes the engine can open; anything else never reaches `checkout`. */
export function isCheckoutUrl(url: string): boolean {
  return /^(https?|svn|file):\/\/[^\s/]/i.test(url.trim());
}
