import type { FileKind, ItemProps } from "../../../ipc/bindings";

/**
 * Property checks of the Properties dialog, mirroring what the engine (and `svn propset`) refuses
 * so the user sees the problem while typing. The engine stays the authority: whatever it still
 * refuses is shown as its error.
 */

/** Subversion properties that apply to files only (`svn_prop_is_known_svn_file_prop`). */
export const FILE_ONLY_PROPS = [
  "svn:executable",
  "svn:keywords",
  "svn:eol-style",
  "svn:mime-type",
  "svn:needs-lock",
  "svn:special",
] as const;

/** Subversion properties that apply to folders only. */
export const DIR_ONLY_PROPS = [
  "svn:ignore",
  "svn:global-ignores",
  "svn:externals",
  "svn:auto-props",
] as const;

/** Names the engine also accepts on folders (kept for compatibility with older working copies). */
const LEGACY_DIR_PROPS = ["svn:inheritable-auto-props", "svn:inheritable-global-ignores"];

/** Subversion properties that apply to files and folders. */
export const ANY_NODE_PROPS = ["svn:mergeinfo"] as const;

/** Properties whose value is always stored as `*` (svn canonicalizes any value). */
export const BOOLEAN_PROPS: readonly string[] = ["svn:executable", "svn:needs-lock", "svn:special"];

/** Values of `svn:eol-style`. */
export const EOL_STYLES = ["native", "LF", "CRLF", "CR"] as const;

const NAME_RE = /^[A-Za-z:_][A-Za-z0-9:_.-]*$/;

/** Problems a property can have; keys of `propsStrings`. */
export type PropProblem =
  | { key: "propNameEmpty" }
  | { key: "propNameInvalid" }
  | { key: "propUnknownSvn"; name: string }
  | { key: "propFileOnly"; name: string }
  | { key: "propDirOnly"; name: string }
  | { key: "propEolInvalid" }
  | { key: "propEolBinary" }
  | { key: "propMimeInvalid"; detail: string }
  | { key: "propExternalsInvalid"; line: string }
  | { key: "propMergeinfoInvalid"; line: string };

/** Subversion property names suggested for an item of `kind`. */
export function knownPropsFor(kind: FileKind): string[] {
  return kind === "dir"
    ? [...DIR_ONLY_PROPS, ...ANY_NODE_PROPS]
    : [...FILE_ONLY_PROPS.filter((p) => p !== "svn:special"), ...ANY_NODE_PROPS];
}

export function isKnownSvnProp(name: string): boolean {
  return (
    (FILE_ONLY_PROPS as readonly string[]).includes(name) ||
    (DIR_ONLY_PROPS as readonly string[]).includes(name) ||
    LEGACY_DIR_PROPS.includes(name) ||
    (ANY_NODE_PROPS as readonly string[]).includes(name)
  );
}

/** `svn_mime_type_validate`; returns what is wrong, or null. */
export function mimeTypeProblem(value: string): string | null {
  const tspecials = '()<>@,;:\\"/[]?=';
  const cut = value.search(/[; ]/);
  const media = cut < 0 ? value : value.slice(0, cut);
  if (media.length === 0) return "empty media type";
  const slash = media.indexOf("/");
  if (slash < 0) return "no '/'";
  for (let i = 0; i < media.length; i++) {
    const c = media.charAt(i);
    const code = c.charCodeAt(0);
    if (
      i !== slash &&
      (code > 127 || code < 32 || code === 127 || /\s/.test(c) || tspecials.includes(c))
    )
      return `'${c}'`;
  }
  for (const c of value) {
    const code = c.charCodeAt(0);
    if ((code < 32 && c !== "\t") || code === 127) return `0x${code.toString(16).padStart(2, "0")}`;
  }
  return null;
}

/** `svn_mime_type_is_binary`. */
export function isBinaryMimeType(value: string): boolean {
  const media = (value.split(";")[0] ?? "").trim();
  return !(media.startsWith("text/") || media === "image/x-xbitmap" || media === "image/x-xpixmap");
}

/** `scheme://` with a scheme svn accepts. */
function isAbsoluteUrl(s: string): boolean {
  return /^[A-Za-z][A-Za-z0-9+.-]*:\/\//.test(s);
}

/**
 * First line of an `svn:externals` value svn would refuse, or null. Mirrors the engine's parser
 * (`svn_wc_parse_externals_description3`): 2 to 4 words, an optional `-r N`, at most one absolute
 * URL, a relative target without `..`, each target once.
 */
export function externalsProblem(value: string): string | null {
  const seen = new Set<string>();
  for (const raw of value.split(/\r?\n/)) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const words = (line.match(/"[^"]*"|'[^']*'|\S+/g) ?? []).map((w) =>
      w.replace(/^(["'])(.*)\1$/, "$2"),
    );
    if (words.length < 2 || words.length > 4) return line;
    let revAt: number | null = null;
    for (let i = 0; i < 2 && i < words.length; i++) {
      const w = words[i] as string;
      if (!w.startsWith("-r")) continue;
      const rev = w === "-r" ? words[i + 1] : w.slice(2);
      if (rev === undefined || !/^\d+$/.test(rev)) return line;
      words.splice(i, w === "-r" ? 2 : 1);
      revAt = i;
      break;
    }
    if (words.length !== 2) return line;
    const [t0, t1] = words as [string, string];
    const [u0, u1] = [isAbsoluteUrl(t0), isAbsoluteUrl(t1)];
    if (u0 && u1) return line;
    if ((revAt === 0 && u1) || (revAt === 1 && u0)) return line;
    const target = revAt === 0 || (revAt === null && (u0 || !u1)) ? t1 : t0;
    const parts = target.split("/").filter((p) => p !== "" && p !== ".");
    if (target.startsWith("/") || parts.length === 0 || parts.includes("..")) return line;
    const key = parts.join("/");
    if (seen.has(key)) return line;
    seen.add(key);
  }
  return null;
}

/** First line of an `svn:mergeinfo` value svn would refuse, or null (`svn_mergeinfo_parse`). */
export function mergeinfoProblem(value: string): string | null {
  for (const line of value.split(/\r?\n/)) {
    if (line === "") continue;
    const colon = line.lastIndexOf(":");
    if (colon < 0) return line;
    const path = line.slice(0, colon);
    const ranges = line.slice(colon + 1).trim();
    if (!path.startsWith("/") || ranges === "") return line;
    for (const part of ranges.split(",")) {
      const range = part.trim().replace(/\*$/, "");
      const m = /^(\d+)(?:-(\d+))?$/.exec(range);
      if (!m) return line;
      const a = Number(m[1]);
      const b = m[2] === undefined ? null : Number(m[2]);
      if (a === 0 || b === 0 || (b !== null && a >= b)) return line;
    }
  }
  return null;
}

/**
 * What is wrong with setting `name` to `value` on an item of `kind` whose current properties are
 * `current`, or null. Deleting is never checked (svn allows deleting any property).
 */
export function propProblem(
  name: string,
  value: string,
  kind: FileKind,
  current: ItemProps["props"] = [],
): PropProblem | null {
  const n = name.trim();
  if (n === "") return { key: "propNameEmpty" };
  if (!NAME_RE.test(n)) return { key: "propNameInvalid" };
  if (!n.startsWith("svn:")) return null;
  if (!isKnownSvnProp(n)) return { key: "propUnknownSvn", name: n };
  const isDir = kind === "dir";
  if (isDir && (FILE_ONLY_PROPS as readonly string[]).includes(n))
    return { key: "propFileOnly", name: n };
  if (!isDir && ((DIR_ONLY_PROPS as readonly string[]).includes(n) || LEGACY_DIR_PROPS.includes(n)))
    return { key: "propDirOnly", name: n };
  switch (n) {
    case "svn:eol-style": {
      if (!(EOL_STYLES as readonly string[]).includes(value.trim()))
        return { key: "propEolInvalid" };
      const mime = current.find((p) => p.name === "svn:mime-type");
      return mime && isBinaryMimeType(mime.value) ? { key: "propEolBinary" } : null;
    }
    case "svn:mime-type": {
      const detail = mimeTypeProblem(value.trim());
      return detail ? { key: "propMimeInvalid", detail } : null;
    }
    case "svn:externals": {
      const line = externalsProblem(value);
      return line === null ? null : { key: "propExternalsInvalid", line };
    }
    case "svn:mergeinfo": {
      const line = mergeinfoProblem(value);
      return line === null ? null : { key: "propMergeinfoInvalid", line };
    }
    default:
      return null;
  }
}

/** One row of the dialog: a property name across the selected items. */
export interface PropRow {
  name: string;
  /** The value when every item that has the property has the same one; null when they differ. */
  value: string | null;
  /** How many of the items have it. */
  count: number;
  /** Some item has a value that is not text: shown, never edited. */
  binary: boolean;
}

/** Rows of the properties of `items`, sorted by name. */
export function propRows(items: readonly ItemProps[]): PropRow[] {
  const rows = new Map<string, PropRow>();
  for (const item of items) {
    for (const p of item.props) {
      const row = rows.get(p.name);
      if (!row) {
        rows.set(p.name, { name: p.name, value: p.value, count: 1, binary: p.binary });
      } else {
        row.count += 1;
        row.binary ||= p.binary;
        if (row.value !== p.value) row.value = null;
      }
    }
  }
  return [...rows.values()].sort((a, b) => a.name.localeCompare(b.name));
}
