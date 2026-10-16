import { expect, test } from "vitest";
import type { SparseDepth } from "../../../ipc/bindings";
import {
  applyEntries,
  byPath,
  checkoutEntries,
  choicesFromWc,
  choose,
  effective,
  parentOf,
} from "./sparse-model";

const m = (pairs: [string, SparseDepth][]) => new Map(pairs);

test("paths: parent and engine order (parents first)", () => {
  expect(parentOf("")).toBeNull();
  expect(parentOf("a")).toBe("");
  expect(parentOf("a/b")).toBe("a");
  expect(["a/b", "b", "", "a", "a-b"].sort(byPath)).toEqual(["", "a", "a/b", "a-b", "b"]);
});

test("effective depth follows the parent unless a folder has its own choice", () => {
  const none = m([]);
  expect(effective("a", "infinity", none)).toBe("infinity");
  expect(effective("a", "immediates", none)).toBe("empty");
  expect(effective("a/b", "immediates", none)).toBe("out");
  expect(effective("a", "files", none)).toBe("out");
  // A choice deep below brings its parents in empty.
  const deep = m([["a/b/c", "infinity"]]);
  expect(effective("a", "empty", deep)).toBe("empty");
  expect(effective("a/b", "empty", deep)).toBe("empty");
  expect(effective("a/b/c/d", "empty", deep)).toBe("infinity");
  expect(effective("x", "empty", deep)).toBe("out");
  // Excluded, unless something below is chosen.
  expect(effective("a", "infinity", m([["a", "exclude"]]))).toBe("out");
  expect(
    effective(
      "a",
      "infinity",
      m([
        ["a", "exclude"],
        ["a/b", "files"],
      ]),
    ),
  ).toBe("empty");
});

test("choose keeps only choices that change something", () => {
  let c = choose(new Map(), "infinity", "a", "infinity");
  expect([...c]).toEqual([]);
  c = choose(c, "infinity", "a", "files");
  expect([...c]).toEqual([["a", "files"]]);
  c = choose(c, "infinity", "a/b", "exclude"); // already out below `files`
  expect([...c]).toEqual([["a", "files"]]);
  c = choose(c, "infinity", "a", null);
  expect([...c]).toEqual([]);
  // A parent choice of `empty` is implied by a choice below it.
  c = choose(new Map([["a/b", "infinity"]]), "empty", "a", "empty");
  expect([...c]).toEqual([["a/b", "infinity"]]);
});

test("checkout entries are the choices, parents first, without the root", () => {
  expect(
    checkoutEntries(
      m([
        ["b", "empty"],
        ["a/x", "files"],
        ["a", "immediates"],
      ]),
    ),
  ).toEqual([
    { path: "a", depth: "immediates" },
    { path: "a/x", depth: "files" },
    { path: "b", depth: "empty" },
  ]);
});

test("an existing working copy becomes the choices that differ from their parent", () => {
  const { rootDepth, choices } = choicesFromWc([
    { path: "", depth: "empty" },
    { path: "trunk", depth: "empty" },
    { path: "trunk/src", depth: "infinity" },
    { path: "trunk/src/deep", depth: "infinity" },
    { path: "tags", depth: "exclude" },
  ]);
  expect(rootDepth).toBe("empty");
  expect([...choices]).toEqual([["trunk/src", "infinity"]]);
});

test("apply: only changed folders, each followed by the choices below it", () => {
  const before = { rootDepth: "empty" as const, choices: m([["trunk/src", "infinity"]]) };
  // Unchanged.
  expect(applyEntries(before, before)).toEqual([]);
  // A new folder elsewhere.
  expect(
    applyEntries(before, {
      rootDepth: "empty",
      choices: m([
        ["trunk/src", "infinity"],
        ["tags", "empty"],
      ]),
    }),
  ).toEqual([{ path: "tags", depth: "empty" }]);
  // A parent that changes is set first, and what it had below is chosen again after it.
  expect(
    applyEntries(before, {
      rootDepth: "empty",
      choices: m([
        ["trunk", "files"],
        ["trunk/src", "infinity"],
      ]),
    }),
  ).toEqual([
    { path: "trunk", depth: "files" },
    { path: "trunk/src", depth: "infinity" },
  ]);
  // Leaving out a folder whose parent stays excludes it.
  const full = { rootDepth: "infinity" as const, choices: m([]) };
  expect(applyEntries(full, { rootDepth: "infinity", choices: m([["a/b", "exclude"]]) })).toEqual([
    { path: "a/b", depth: "exclude" },
  ]);
  // The root changing covers everything.
  expect(applyEntries(full, { rootDepth: "files", choices: m([["docs", "immediates"]]) })).toEqual([
    { path: "", depth: "files" },
    { path: "docs", depth: "immediates" },
  ]);
  // Dropping a deep choice under an empty parent: the parent that leaves is excluded.
  expect(applyEntries(before, { rootDepth: "empty", choices: m([]) })).toEqual([
    { path: "trunk", depth: "exclude" },
  ]);
});
