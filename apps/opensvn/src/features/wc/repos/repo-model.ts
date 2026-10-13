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

export type FolderNameError =
  | "empty"
  | "tooLong"
  | "separator"
  | "dots"
  | "format"
  | "chars"
  | "trailing"
  | "admin"
  | "reserved";

/** Longest folder name the backend accepts, in UTF-8 bytes. */
const MAX_NAME_BYTES = 255;

/** Windows device names, with the digit forms Windows also reserves (0-9 and superscripts). */
const RESERVED = /^(con|prn|aux|nul|conin\$|conout\$|(com|lpt)[0-9¹²³])$/i;

/**
 * Invisible or text-reordering characters: every Unicode format character (Cf: zero-width and
 * bidirectional controls, the BOM, soft hyphen, Arabic/Syriac prepended marks, interlinear
 * annotation, musical symbol format controls, tag characters, ...), line and paragraph separators
 * and blank-looking fillers. A name containing them can display as a different name. Same set as
 * the backend's `display_text::is_deceptive` (C0/C1 controls are refused separately as "chars").
 */
const FORMAT_CHARS =
  // U+034F (combining grapheme joiner) sits outside the class: a class may not mix in combining marks.
  /\u034F|[\u00AD\u0600-\u0605\u061C\u06DD\u070F\u08E2\u115F\u1160\u17B4\u17B5\u180E\u200B-\u200F\u2028-\u202E\u2060-\u206F\u3164\uFEFF\uFFA0\uFFF9-\uFFFB\u{110BD}\u{110CD}\u{13430}-\u{1343F}\u{1BCA0}-\u{1BCA3}\u{1D173}-\u{1D17A}\u{E0000}-\u{E007F}]/u;

/** Code points some file systems ignore when they compare names. */
const FS_IGNORABLE = /[\u200C-\u200F\u202A-\u202E\u206A-\u206F\uFEFF]/gu;

/**
 * Whether `name` would be taken for a Subversion administrative folder (`.svn`, `_svn`, the NTFS
 * short name `SVN~1`), after compatibility normalization, case folding and dropping the trailing
 * dots and spaces Windows ignores.
 */
export function isAdminFolderName(name: string): boolean {
  const folded = name
    .normalize("NFKC")
    .replace(FS_IGNORABLE, "")
    .toUpperCase()
    .toLowerCase()
    .replace(/[. ]+$/, "");
  return folded === ".svn" || folded === "_svn" || /^svn~\d+$/.test(folded);
}

/**
 * Mirrors the backend check of the folder a checkout creates (one portable path component). The
 * backend validates again; this only gives early feedback.
 */
export function validateFolderName(name: string): FolderNameError | null {
  if (name.length === 0) return "empty";
  if (new TextEncoder().encode(name).length > MAX_NAME_BYTES) return "tooLong";
  if (/[\\/]/.test(name)) return "separator";
  if (name === "." || name === "..") return "dots";
  if (FORMAT_CHARS.test(name)) return "format";
  // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are what we refuse
  if (/[<>:"|?*\u0000-\u001f\u007f-\u009f]/.test(name)) return "chars";
  if (/[. ]$/.test(name) || name.startsWith(" ")) return "trailing";
  if (isAdminFolderName(name)) return "admin";
  const stem = (name.split(".")[0] ?? name).trimEnd();
  if (RESERVED.test(stem)) return "reserved";
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
    .replace(/[\\/<>:"|?*\u0000-\u001f\u007f-\u009f]/g, "-")
    .replace(new RegExp(FORMAT_CHARS.source, "gu"), "")
    .replace(/[. ]+$/, "")
    .trimStart();
  if (validateFolderName(name) !== null) return "";
  return name;
}

/** Joins a parent label and a child name for display only. */
export function joinDisplay(parent: string, name: string): string {
  if (!name) return parent;
  const sep = parent.includes("\\") && !parent.includes("/") ? "\\" : "/";
  return parent.endsWith(sep) ? `${parent}${name}` : `${parent}${sep}${name}`;
}
