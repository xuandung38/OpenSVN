import { compareListings } from "../../features/wc/compare/compare-model";
import type {
  CompareEntry,
  CompareRequest,
  CompareSummary,
  FileStatus,
  WorkingCopy,
} from "../bindings";
import { MOCK_REPO_ROOT, MOCK_YOUNGEST, mockList } from "./remote-data";

const join = (base: string, path: string) => (path ? `${base}/${path}` : base);

/** Everything below `url` in the mock tree, depth first. */
function below(url: string, path: string): { path: string; kind: "file" | "dir" }[] {
  const out: { path: string; kind: "file" | "dir" }[] = [];
  for (const e of mockList(join(url, path))?.entries ?? []) {
    const p = path ? `${path}/${e.name}` : e.name;
    out.push({ path: p, kind: e.kind });
    if (e.kind === "dir") out.push(...below(url, p));
  }
  return out;
}

/**
 * `compare_summarize` over the mock tree: listings compared by kind, last-changed revision and
 * size, like the old folder-by-folder Compare; local changes come from the mock status. The mock
 * has no history, so revisions only show in the answer (same URL and revision: no differences).
 */
export function mockCompareSummary(
  req: CompareRequest,
  wc: WorkingCopy,
  status: readonly FileStatus[],
): CompareSummary | string {
  const right = req.rightUrl.replace(/\/+$/, "");
  if (right !== MOCK_REPO_ROOT && !right.startsWith(`${MOCK_REPO_ROOT}/`)) {
    return `${req.rightUrl} is not in this repository; compare folders of one repository.`;
  }
  if (mockList(right) === null) return `${req.rightUrl} does not exist.`;
  const leftRev = req.includeLocal ? null : (req.leftRev ?? MOCK_YOUNGEST);
  const rightRev = req.rightRev ?? MOCK_YOUNGEST;
  const entries: CompareEntry[] = [];
  const add = (path: string, kind: "file" | "dir", change: CompareEntry["change"]) =>
    entries.push({ path, kind, change, propsChanged: false });
  const same = right === wc.url.replace(/\/+$/, "") && !req.includeLocal && leftRev === rightRev;
  const walk = (path: string) => {
    const l = mockList(join(wc.url, path))?.entries ?? [];
    const r = mockList(join(right, path))?.entries ?? [];
    for (const row of compareListings(l, r)) {
      const p = path ? `${path}/${row.name}` : row.name;
      if (row.state === "onlyLeft" || row.state === "kindChanged") {
        add(p, row.left?.kind ?? "file", "deleted");
        if (row.left?.kind === "dir")
          for (const c of below(wc.url, p)) add(c.path, c.kind, "deleted");
      }
      if (row.state === "onlyRight" || row.state === "kindChanged") {
        add(p, row.right?.kind ?? "file", "added");
        if (row.right?.kind === "dir")
          for (const c of below(right, p)) add(c.path, c.kind, "added");
      }
      if (row.state === "changed") {
        if (row.kind === "dir") walk(p);
        else add(p, "file", "modified");
      }
    }
  };
  if (!same) walk("");
  if (req.includeLocal) {
    for (const f of status) {
      const known = entries.find((e) => e.path === f.path);
      if (f.text === "added" && !known) add(f.path, f.kind === "dir" ? "dir" : "file", "deleted");
      else if (f.text === "deleted" && !known)
        add(f.path, f.kind === "dir" ? "dir" : "file", "added");
      else if ((f.text === "modified" || f.props === "modified") && !known) {
        entries.push({
          path: f.path,
          kind: f.kind === "dir" ? "dir" : "file",
          change: f.text === "modified" ? "modified" : "normal",
          propsChanged: f.props === "modified",
        });
      }
    }
  }
  entries.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  return { leftRev, rightRev, entries };
}
