/** URL helpers for repository locations (https:// URLs only; never local paths). */

const trimSlash = (s: string) => s.replace(/\/+$/, "");

/** Appends one child name, percent-encoding it as a single path segment. */
export function joinUrl(base: string, name: string): string {
  return `${trimSlash(base)}/${encodeURIComponent(name)}`;
}

/** Parent URL, never above `root`. */
export function parentUrl(url: string, root: string): string {
  const u = trimSlash(url);
  const r = trimSlash(root);
  if (u === r || !u.startsWith(r)) return r;
  const i = u.lastIndexOf("/");
  return i <= r.length ? r : u.slice(0, i);
}

/** Last path segment, decoded for display. */
export function baseName(url: string): string {
  const seg = trimSlash(url).split("/").pop() ?? "";
  try {
    return decodeURIComponent(seg);
  } catch {
    return seg;
  }
}

/** Breadcrumb segments from `root` down to `url` (inclusive); first element is the root itself. */
export function crumbs(url: string, root: string): { label: string; url: string }[] {
  const r = trimSlash(root);
  const out = [{ label: baseName(r) || r, url: r }];
  const u = trimSlash(url);
  if (!u.startsWith(r) || u === r) return out;
  let cur = r;
  for (const seg of u.slice(r.length).split("/").filter(Boolean)) {
    cur = `${cur}/${seg}`;
    out.push({ label: decodeURIComponent(seg), url: cur });
  }
  return out;
}

/** Path of `url` inside the repository, as `/a/b` (root = `/`), decoded for display. */
export function repoPath(url: string, repoRoot: string): string {
  const u = trimSlash(url);
  const r = trimSlash(repoRoot);
  if (!u.startsWith(r)) return u;
  const rel = u.slice(r.length);
  try {
    return decodeURI(rel) || "/";
  } catch {
    return rel || "/";
  }
}

/** URL for a repository-absolute path like `/trunk/a b.ts`. */
export function urlFromRepoPath(repoRoot: string, path: string): string {
  const segs = path.split("/").filter(Boolean).map(encodeURIComponent);
  return [trimSlash(repoRoot), ...segs].join("/");
}

/** Validates a single folder/file name for server-side create/rename. Returns an error key or null. */
export function validateEntryName(name: string): "empty" | "slash" | "dots" | "chars" | null {
  const n = name.trim();
  if (n.length === 0) return "empty";
  if (n.includes("/") || n.includes("\\")) return "slash";
  if (n === "." || n === "..") return "dots";
  // biome-ignore lint/suspicious/noControlCharactersInRegex: control characters are exactly what is rejected
  if (/[\u0000-\u001f:*?"<>|]/.test(n)) return "chars";
  return null;
}
