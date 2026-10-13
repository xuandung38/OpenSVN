import type { BranchNode, Layout } from "../../../ipc/bindings";
import { joinUrl, parentUrl, urlFromRepoPath } from "../repo-url";

const LAYOUT_RE = /^(.*?)\/(trunk|branches\/[^/]+|tags\/[^/]+)(\/.*)?$/i;

/**
 * Project root of a layout URL: the part before `/trunk`, `/branches/<name>` or `/tags/<name>`.
 * `null` when the URL does not follow the convention (the dialog then falls back to Location mode).
 */
export function projectRootOf(url: string): string | null {
  const m = LAYOUT_RE.exec(url.replace(/\/+$/, ""));
  return m ? (m[1] ?? null) : null;
}

/** Parses the revision field: empty/`HEAD` = HEAD (null), a positive integer = that revision, else `"invalid"`. */
export function parseRevision(text: string): number | null | "invalid" {
  const s = text.trim();
  if (s === "" || s.toUpperCase() === "HEAD") return null;
  if (!/^\d+$/.test(s)) return "invalid";
  const n = Number(s);
  return Number.isSafeInteger(n) && n > 0 ? n : "invalid";
}

export interface LayoutContext {
  /** Backend layout for the repository (`layout_get`), if loaded. */
  layout: Layout | null;
  /** Known trunk/branch/tag nodes (`branches_list`). */
  nodes: BranchNode[];
  /** URL of the working copy's current branch/tag, if any. */
  currentRefUrl: string | null;
  /** URL being copied. */
  sourceUrl: string;
  repoRootUrl: string;
}

const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Matches `url` against layout patterns like `/{project}/branches` and returns the `{project}` value. */
function projectFromPatterns(url: string, root: string, patterns: string[]): string | null {
  const path = url.slice(root.replace(/\/+$/, "").length);
  for (const p of patterns) {
    const re = new RegExp(`^${escapeRe(p).replace("\\{project\\}", "([^/]+)")}(?:/|$)`, "i");
    const m = re.exec(path);
    if (m) return m[1] ?? "";
  }
  return null;
}

/**
 * Folder that holds new branches/tags (`.../branches` or `.../tags`). Uses what the backend knows:
 * existing branch/tag nodes first, then the configured custom layout, and only as a last resort the
 * trunk/branches/tags naming convention (auto layout of a repo with no branches yet).
 * `null` means the destination cannot be derived and the dialog falls back to Location mode.
 */
export function refContainerUrl(kind: "branch" | "tag", ctx: LayoutContext): string | null {
  const wanted = kind === "branch" ? "branch" : "tag";
  const node = ctx.nodes.find((n) => n.kind === wanted);
  if (node) return parentUrl(node.url, ctx.repoRootUrl);

  if (ctx.layout?.mode === "none") return null;
  if (ctx.layout?.mode === "custom") {
    const containers = kind === "branch" ? ctx.layout.branches : ctx.layout.tags;
    const first = containers[0];
    if (first) {
      if (!first.includes("{project}")) return urlFromRepoPath(ctx.repoRootUrl, first);
      const all = [ctx.layout.trunk, ...ctx.layout.branches, ...ctx.layout.tags];
      const project =
        projectFromPatterns(ctx.currentRefUrl ?? ctx.sourceUrl, ctx.repoRootUrl, all) ??
        projectFromPatterns(ctx.sourceUrl, ctx.repoRootUrl, all);
      return project ? urlFromRepoPath(ctx.repoRootUrl, first.replace("{project}", project)) : null;
    }
    return null;
  }
  const root = projectRootOf(ctx.currentRefUrl ?? ctx.sourceUrl) ?? projectRootOf(ctx.sourceUrl);
  return root ? joinUrl(root, kind === "branch" ? "branches" : "tags") : null;
}
