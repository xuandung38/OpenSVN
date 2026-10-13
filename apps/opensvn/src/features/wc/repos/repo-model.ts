import type { WcEntry } from "../../../ipc/bindings";

/** Last `n` components of a local path, for display ("…/work/game-client"). */
export function pathTail(path: string, n = 2): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  if (parts.length <= n) return path;
  return `…/${parts.slice(-n).join("/")}`;
}

/** Host of a repository URL ("svn.example.com"), or the URL itself when it does not parse. */
export function urlHost(url: string): string {
  try {
    const u = new URL(url);
    return u.host || u.protocol.replace(/:$/, "");
  } catch {
    return url;
  }
}

/** Case-insensitive match on display name, folder and repository URL. */
export function filterEntries(entries: readonly WcEntry[], query: string): WcEntry[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...entries];
  return entries.filter((e) =>
    [e.name, e.rootPath, e.repoRootUrl, e.url ?? ""].some((s) => s.toLowerCase().includes(q)),
  );
}

/** Most recently opened first; never-opened entries keep their list order at the end. */
export function sortByLastOpened(entries: readonly WcEntry[]): WcEntry[] {
  return [...entries].sort((a, b) => (b.lastOpenedAt ?? "").localeCompare(a.lastOpenedAt ?? ""));
}

export type FolderNameError = "empty" | "separator" | "dots" | "chars" | "trailing" | "reserved";

const RESERVED = /^(con|prn|aux|nul|conin\$|conout\$|(com|lpt)[1-9¹²³])$/i;

/**
 * Mirrors the backend check of the folder a checkout creates (one portable path component). The
 * backend validates again; this only gives early feedback.
 */
export function validateFolderName(name: string): FolderNameError | null {
  if (name.length === 0) return "empty";
  if (/[\\/]/.test(name)) return "separator";
  if (name === "." || name === "..") return "dots";
  // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are what we refuse
  if (/[<>:"|?*\u0000-\u001f\u007f]/.test(name)) return "chars";
  if (/[. ]$/.test(name) || name.startsWith(" ")) return "trailing";
  const stem = (name.split(".")[0] ?? name).trimEnd();
  if (RESERVED.test(stem)) return "reserved";
  if (new TextEncoder().encode(name).length > 255) return "chars";
  return null;
}

/**
 * Folder name suggested for a checkout URL: the project for `…/project/trunk`, otherwise the last
 * path segment, made safe for every file system.
 */
export function suggestFolderName(url: string): string {
  let path: string;
  try {
    path = decodeURIComponent(new URL(url.trim()).pathname);
  } catch {
    return "";
  }
  const segs = path.split("/").filter(Boolean);
  let name = segs.at(-1) ?? "";
  if (name.toLowerCase() === "trunk" && segs.length > 1) name = segs.at(-2) ?? name;
  name = name
    // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are replaced
    .replace(/[\\/<>:"|?*\u0000-\u001f\u007f]/g, "-")
    .replace(/[. ]+$/, "")
    .trimStart();
  if (name === "." || name === ".." || validateFolderName(name) === "reserved") return "";
  return name;
}

/** Joins a parent label and a child name for display only. */
export function joinDisplay(parent: string, name: string): string {
  if (!name) return parent;
  const sep = parent.includes("\\") && !parent.includes("/") ? "\\" : "/";
  return parent.endsWith(sep) ? `${parent}${name}` : `${parent}${sep}${name}`;
}
