import type { Layout } from "../../../ipc/bindings";

/** Problems of a custom layout; keys of `propsStrings`. */
export type LayoutProblem = "layoutTrunkRequired" | "layoutDots" | "layoutTooMany";

/** Folders of a textarea: one per line, trimmed, with a leading `/`, empty lines dropped. */
export function folderLines(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l !== "")
    .map(normalizeFolder);
}

/** `trunk/` and `/trunk` are the same folder; patterns are kept with one leading `/`. */
export function normalizeFolder(folder: string): string {
  const f = folder.trim().replace(/\/+$/, "");
  return f.startsWith("/") ? f : `/${f}`;
}

/**
 * What the backend (`layout_valid`) would refuse in a custom layout, or null: an empty trunk,
 * `..`, a NUL, more than 500 bytes, or more than 20 branch or tag folders.
 */
export function layoutProblem(layout: Layout): LayoutProblem | null {
  if (layout.mode !== "custom") return null;
  if (layout.trunk.trim() === "" || layout.trunk.trim() === "/") return "layoutTrunkRequired";
  if (layout.branches.length > 20 || layout.tags.length > 20) return "layoutTooMany";
  const bad = (p: string) =>
    p.includes("..") || p.includes("\0") || new TextEncoder().encode(p).length > 500;
  if ([layout.trunk, ...layout.branches, ...layout.tags].some(bad)) return "layoutDots";
  return null;
}

/** Short text of a layout for "Detected: …". */
export function describeLayout(layout: Layout): string {
  switch (layout.mode) {
    case "custom":
      return [layout.trunk, ...layout.branches, ...layout.tags].join(", ");
    case "none":
      return "—";
    case "auto":
      return "auto";
  }
}
