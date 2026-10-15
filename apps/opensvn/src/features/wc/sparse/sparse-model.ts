import type { Depth, SparseDepth, SparseEntry } from "../../../ipc/bindings";

/**
 * Per-folder depths of the "Choose folders" dialog. Paths are relative to the checkout root
 * (`""` is the root), with `/` between names.
 *
 * The model follows svn 1.15: setting the depth of a folder leaves it as a fresh checkout at that
 * depth would (what it had below is reset), and a folder below one the working copy does not have
 * is brought in with its missing parents at depth `empty` (`svn update --parents --set-depth`).
 * The engine applies the entries parents first, so a choice below a folder that also changes
 * depth is applied after it.
 */

/** What a folder ends up as: a depth, or not in the working copy. */
export type Effective = Depth | "out";

/** Explicit choices by path; folders without one follow their parent. */
export type Choices = ReadonlyMap<string, SparseDepth>;

export function parentOf(path: string): string | null {
  if (path === "") return null;
  const i = path.lastIndexOf("/");
  return i < 0 ? "" : path.slice(0, i);
}

export function joinPath(parent: string, name: string): string {
  return parent === "" ? name : `${parent}/${name}`;
}

/** What a child folder of a folder at `parent` gets without a choice of its own. */
export function implied(parent: Effective): Effective {
  switch (parent) {
    case "infinity":
      return "infinity";
    case "immediates":
      return "empty";
    default:
      return "out";
  }
}

/**
 * The effective depth of `path` given the root depth and the choices. A folder with a choice of
 * its own comes in even below a folder that would leave it out (its parents come in empty).
 */
export function effective(path: string, rootDepth: Depth, choices: Choices): Effective {
  if (path === "") return choices.get("") === undefined ? rootDepth : toEffective(choices.get(""));
  const own = choices.get(path);
  // An excluded folder with a choice below comes back empty (that choice brings its parents).
  if (own === "exclude" && hasChoiceBelow(path, choices)) return "empty";
  if (own !== undefined) return toEffective(own);
  const parent = parentOf(path) ?? "";
  const imp = implied(effective(parent, rootDepth, choices));
  if (imp === "out" && hasChoiceBelow(path, choices)) return "empty";
  return imp;
}

function toEffective(d: SparseDepth | undefined): Effective {
  return d === undefined || d === "exclude" ? "out" : d;
}

/** Whether a folder strictly below `path` has a choice that brings it in. */
export function hasChoiceBelow(path: string, choices: Choices): boolean {
  const prefix = path === "" ? "" : `${path}/`;
  for (const [p, d] of choices) {
    if (p !== path && p.startsWith(prefix) && d !== "exclude") return true;
  }
  return false;
}

/** Path order of the engine: parents first, then names in order. */
export function byPath(a: string, b: string): number {
  const pa = a === "" ? [] : a.split("/");
  const pb = b === "" ? [] : b.split("/");
  for (let i = 0; i < Math.min(pa.length, pb.length); i++) {
    const x = pa[i] as string;
    const y = pb[i] as string;
    if (x !== y) return x < y ? -1 : 1;
  }
  return pa.length - pb.length;
}

/**
 * Sets (or with `null`, clears) the choice of `path`, keeping only choices that change something:
 * a choice equal to what the folder would get anyway is dropped.
 */
export function choose(
  choices: Choices,
  rootDepth: Depth,
  path: string,
  depth: SparseDepth | null,
): Map<string, SparseDepth> {
  const next = new Map(choices);
  if (depth === null) next.delete(path);
  else next.set(path, depth);
  return prune(next, rootDepth);
}

/** Drops the choices that equal what their folder would get without them. */
export function prune(
  choices: Map<string, SparseDepth>,
  rootDepth: Depth,
): Map<string, SparseDepth> {
  for (const path of [...choices.keys()].sort(byPath).reverse()) {
    if (path === "") continue;
    const d = choices.get(path) as SparseDepth;
    const without = new Map(choices);
    without.delete(path);
    const would = effective(path, rootDepth, without);
    if (would === toEffective(d)) choices.delete(path);
  }
  return choices;
}

/** The entries of a new checkout: every choice, parents first (the root takes `rootDepth`). */
export function checkoutEntries(choices: Choices): SparseEntry[] {
  return [...choices]
    .filter(([p]) => p !== "")
    .sort(([a], [b]) => byPath(a, b))
    .map(([path, depth]) => ({ path, depth }));
}

/**
 * Choices that describe an existing working copy (`wc_depths`): only what differs from what each
 * folder's parent implies. The first value is the root depth.
 */
export function choicesFromWc(depths: readonly SparseEntry[]): {
  rootDepth: Depth;
  choices: Map<string, SparseDepth>;
} {
  const root = depths.find((e) => e.path === "");
  const rootDepth: Depth = root && root.depth !== "exclude" ? root.depth : "infinity";
  const choices = new Map<string, SparseDepth>();
  for (const e of depths) if (e.path !== "") choices.set(e.path, e.depth);
  // Present folders without an entry cannot occur (wc_depths lists them all); a listed folder
  // equal to its implied depth needs no choice.
  return { rootDepth, choices: prune(choices, rootDepth) };
}

/**
 * The steps that turn the working copy `before` into `after`, for `wc_sparse_apply`: each folder
 * whose effective depth changes (and no ancestor of which changes) is set, followed by every choice
 * below it (setting a folder resets what it had below). A folder without a choice on either side
 * only changes through an ancestor, so the choices and their ancestors are all that is compared.
 */
export function applyEntries(
  before: { rootDepth: Depth; choices: Choices },
  after: { rootDepth: Depth; choices: Choices },
): SparseEntry[] {
  const set = new Set<string>([""]);
  for (const p of [...before.choices.keys(), ...after.choices.keys()]) {
    for (let q: string | null = p; q !== null; q = parentOf(q)) set.add(q);
  }
  const paths = [...set].sort(byPath);
  const changed = paths.filter(
    (p) =>
      effective(p, before.rootDepth, before.choices) !==
      effective(p, after.rootDepth, after.choices),
  );
  const tops = changed.filter(
    (p) => !changed.some((q) => q !== p && (q === "" || p.startsWith(`${q}/`))),
  );
  const out = new Map<string, SparseDepth>();
  for (const top of tops) {
    const eff = effective(top, after.rootDepth, after.choices);
    const parent = parentOf(top);
    // A folder that leaves while its parent stays is excluded; below a folder that leaves too,
    // nothing needs saying.
    if (eff === "out") {
      if (parent !== null && effective(parent, after.rootDepth, after.choices) !== "out") {
        out.set(top, "exclude");
      }
      continue;
    }
    out.set(top, eff);
    const prefix = top === "" ? "" : `${top}/`;
    for (const [p, d] of after.choices) {
      if (p !== top && p.startsWith(prefix)) out.set(p, d);
    }
  }
  return [...out].sort(([a], [b]) => byPath(a, b)).map(([path, depth]) => ({ path, depth }));
}
