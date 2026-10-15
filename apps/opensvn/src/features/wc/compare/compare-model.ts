import type { RepoEntry } from "../../../ipc/bindings";

/** How one entry of a folder differs between the current location (left) and the other (right). */
export type CompareState = "same" | "changed" | "onlyLeft" | "onlyRight" | "kindChanged";

export interface CompareRow {
  name: string;
  kind: RepoEntry["kind"];
  state: CompareState;
  left: RepoEntry | null;
  right: RepoEntry | null;
}

/**
 * Compares two folder listings of the same repository by name. An entry is unchanged when both
 * sides have the same kind, last-changed revision and size: a branch made with `svn copy` keeps the
 * source's last-changed revision on every node it has not touched, so a different revision means
 * the node (or, for a folder, something below it) changed on one side. Rows are sorted folders
 * first, then by name.
 */
export function compareListings(
  left: readonly RepoEntry[],
  right: readonly RepoEntry[],
): CompareRow[] {
  const byName = new Map<string, { left: RepoEntry | null; right: RepoEntry | null }>();
  for (const e of left) byName.set(e.name, { left: e, right: null });
  for (const e of right) {
    const cur = byName.get(e.name);
    if (cur) cur.right = e;
    else byName.set(e.name, { left: null, right: e });
  }
  const rows: CompareRow[] = [];
  for (const [name, { left: l, right: r }] of byName) {
    let state: CompareState;
    if (!r) state = "onlyLeft";
    else if (!l) state = "onlyRight";
    else if (l.kind !== r.kind) state = "kindChanged";
    else if (l.createdRev === r.createdRev && l.size === r.size) state = "same";
    else state = "changed";
    rows.push({ name, kind: (l ?? r)?.kind ?? "file", state, left: l, right: r });
  }
  return rows.sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
    return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
  });
}

/** Whether `url` is a folder URL inside the repository rooted at `root` (and not `self`). */
export function isCompareTarget(url: string, root: string, self: string): boolean {
  const u = url.trim().replace(/\/+$/, "");
  const r = root.replace(/\/+$/, "");
  return (
    /^(https?|svn|file):\/\/[^\s/]/i.test(u) &&
    (u === r || u.startsWith(`${r}/`)) &&
    u !== self.replace(/\/+$/, "")
  );
}
