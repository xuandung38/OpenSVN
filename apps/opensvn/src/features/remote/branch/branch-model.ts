import { joinUrl } from "../repo-url";

const LAYOUT_RE = /^(.*?)\/(trunk|branches\/[^/]+|tags\/[^/]+)(\/.*)?$/i;

/**
 * Project root of a layout URL: the part before `/trunk`, `/branches/<name>` or `/tags/<name>`.
 * `null` when the URL does not follow the convention (the dialog then falls back to Location mode).
 */
export function projectRootOf(url: string): string | null {
  const m = LAYOUT_RE.exec(url.replace(/\/+$/, ""));
  return m ? (m[1] ?? null) : null;
}

/** Destination URL of a new branch or tag next to the current one. */
export function refTargetUrl(projectRoot: string, kind: "branch" | "tag", name: string): string {
  return joinUrl(joinUrl(projectRoot, kind === "branch" ? "branches" : "tags"), name.trim());
}

/** Parses the revision field: empty/`HEAD` = HEAD (null), a positive integer = that revision, else `"invalid"`. */
export function parseRevision(text: string): number | null | "invalid" {
  const s = text.trim();
  if (s === "" || s.toUpperCase() === "HEAD") return null;
  if (!/^\d+$/.test(s)) return "invalid";
  const n = Number(s);
  return Number.isSafeInteger(n) && n > 0 ? n : "invalid";
}
