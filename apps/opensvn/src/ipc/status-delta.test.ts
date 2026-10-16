import { QueryClient } from "@tanstack/react-query";
import { expect, test, vi } from "vitest";
import type { FileStatus, StatusSnapshot } from "./bindings";
import { queryKeys } from "./query-keys";
import {
  applyDelta,
  applyStatusChanged,
  comparePaths,
  deltaAction,
  keepLaterSnapshot,
} from "./status-delta";

const file = (path: string, text: FileStatus["text"] = "modified"): FileStatus =>
  ({ path, text }) as FileStatus;

test("paths sort by code point like the backend, not by UTF-16 unit", () => {
  // U+FF5E (BMP, above the surrogates) sorts before U+1F600 by code point but after it in UTF-16.
  const list = ["b", "\u{1F600}", "～", "a/b", "a"];
  expect([...list].sort(comparePaths)).toEqual(["a", "a/b", "b", "～", "\u{1F600}"]);
});

test("a delta is applied to its base, ignored by a later snapshot, refetched otherwise", () => {
  expect(deltaAction(4, { base: 4, snapshot: 5 })).toBe("apply");
  expect(deltaAction(5, { base: 4, snapshot: 5 })).toBe("ignore");
  expect(deltaAction(9, { base: 4, snapshot: 5 })).toBe("ignore");
  expect(deltaAction(3, { base: 4, snapshot: 5 })).toBe("refetch");
  expect(deltaAction(null, { base: 4, snapshot: 5 })).toBe("refetch");
  expect(deltaAction(4, { base: null, snapshot: 5 })).toBe("refetch");
});

test("applying a delta replaces, adds and removes entries in path order", () => {
  const out = applyDelta([file("a"), file("c"), file("d")], {
    changed: [file("c", "added"), file("b")],
    removed: ["d"],
  });
  expect(out.map((f) => [f.path, f.text])).toEqual([
    ["a", "modified"],
    ["b", "modified"],
    ["c", "added"],
  ]);
});

test("status-changed updates the list in place and refetches diffs below the status key", () => {
  const qc = new QueryClient();
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  const listKey = [...queryKeys.status("w"), { includeIgnored: false }];
  const ignoredKey = [...queryKeys.status("w"), { includeIgnored: true }];
  const diffKey = [...queryKeys.status("w"), "diff", "a", false, false];
  qc.setQueryData<StatusSnapshot>(listKey, { snapshot: 7, files: [file("a")] });
  qc.setQueryData<StatusSnapshot>(ignoredKey, { snapshot: null, files: [file("a")] });
  qc.setQueryData(diffKey, { hunks: [] });
  applyStatusChanged(qc, {
    wcId: "w",
    changed: [file("b")],
    removed: ["a"],
    base: 7,
    snapshot: 8,
  });
  expect(qc.getQueryData(listKey)).toEqual({ snapshot: 8, files: [file("b")] });
  expect(invalidate).toHaveBeenCalledWith({ queryKey: ignoredKey, exact: true });
  expect(invalidate).toHaveBeenCalledWith({ queryKey: diffKey, exact: true });
  expect(invalidate).not.toHaveBeenCalledWith({ queryKey: listKey, exact: true });
});

test("a fetch that returns an older snapshot than the one held is dropped", () => {
  const held: StatusSnapshot = { snapshot: 8, files: [file("b")] };
  const stale: StatusSnapshot = { snapshot: 7, files: [file("a")] };
  expect(keepLaterSnapshot(held, stale)).toBe(held);
  const newer: StatusSnapshot = { snapshot: 9, files: [file("c")] };
  expect(keepLaterSnapshot(held, newer)).toEqual(newer);
});
