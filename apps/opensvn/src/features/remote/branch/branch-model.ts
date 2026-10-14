import type { BranchNode, Layout } from "../../../ipc/bindings";
import { joinUrl, parentUrl, urlFromRepoPath } from "../repo-url";

// Case-sensitive, like Subversion paths and the backend detection (`/Trunk` is not trunk).
const LAYOUT_RE = /^(.*?)\/(trunk|branches\/[^/]+|tags\/[^/]+)(\/.*)?$/;

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
    const re = new RegExp(`^${escapeRe(p).replace("\\{project\\}", "([^/]+)")}(?:/|$)`);
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
  if (ctx.layout?.mode === "none") return null;
  const custom = ctx.layout?.mode === "custom" ? ctx.layout : null;
  const patterns = custom ? [custom.trunk, ...custom.branches, ...custom.tags] : [];

  // Which project does the current ref belong to? Branches of other projects are never candidates:
  // in a multi-project repo the first branch in the repository may belong to a different project.
  const projectOf = (url: string): string | null =>
    custom
      ? projectFromPatterns(url, ctx.repoRootUrl, patterns)
      : (projectRootOf(url.replace(/\/+$/, "")) ?? null);
  const currentProject = projectOf(ctx.currentRefUrl ?? ctx.sourceUrl) ?? projectOf(ctx.sourceUrl);

  const wanted = kind === "branch" ? "branch" : "tag";
  const sameKind = ctx.nodes.filter((n) => n.kind === wanted);
  const candidates =
    currentProject === null
      ? sameKind
      : sameKind.filter((n) => projectOf(n.url) === currentProject);
  const containers = [...new Set(candidates.map((n) => parentUrl(n.url, ctx.repoRootUrl)))];
  // Unknown project: only trust existing nodes when they all live in one folder.
  if (containers.length === 1 || (currentProject !== null && containers.length > 0)) {
    return containers[0] as string;
  }

  if (custom) {
    const first = (kind === "branch" ? custom.branches : custom.tags)[0];
    if (!first) return null;
    if (!first.includes("{project}")) return urlFromRepoPath(ctx.repoRootUrl, first);
    return currentProject
      ? urlFromRepoPath(ctx.repoRootUrl, first.replace("{project}", currentProject))
      : null;
  }
  const root = currentProject ?? null;
  return root ? joinUrl(root, kind === "branch" ? "branches" : "tags") : null;
}
