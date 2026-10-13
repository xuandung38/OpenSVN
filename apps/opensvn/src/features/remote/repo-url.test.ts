import { expect, test } from "vitest";
import {
  baseName,
  crumbs,
  joinUrl,
  parentUrl,
  repoPath,
  urlFromRepoPath,
  validateEntryName,
} from "./repo-url";

const ROOT = "https://h/repos/game";

test("joinUrl percent-encodes a single segment", () => {
  expect(joinUrl(`${ROOT}/`, "a b#c.txt")).toBe(`${ROOT}/a%20b%23c.txt`);
  expect(joinUrl(ROOT, "<img src=x>")).toBe(`${ROOT}/%3Cimg%20src%3Dx%3E`);
});

test("parentUrl never goes above the root", () => {
  expect(parentUrl(`${ROOT}/trunk/src`, ROOT)).toBe(`${ROOT}/trunk`);
  expect(parentUrl(`${ROOT}/trunk`, ROOT)).toBe(ROOT);
  expect(parentUrl(ROOT, ROOT)).toBe(ROOT);
  expect(parentUrl("https://other/x", ROOT)).toBe(ROOT);
});

test("baseName, crumbs and repoPath decode for display", () => {
  expect(baseName(`${ROOT}/a%20b`)).toBe("a b");
  expect(crumbs(`${ROOT}/trunk/a%20b`, ROOT).map((c) => c.label)).toEqual(["game", "trunk", "a b"]);
  expect(repoPath(`${ROOT}/trunk/a%20b`, ROOT)).toBe("/trunk/a b");
  expect(repoPath(ROOT, ROOT)).toBe("/");
  expect(urlFromRepoPath(ROOT, "/trunk/a b")).toBe(`${ROOT}/trunk/a%20b`);
});

test.each([
  ["", "empty"],
  ["  ", "empty"],
  ["a/b", "slash"],
  ["a\\b", "slash"],
  ["..", "dots"],
  ["a:b", "chars"],
  ["a\u0001b", "chars"],
  ["fine name.txt", null],
])("validateEntryName(%j) = %s", (name, expected) => {
  expect(validateEntryName(name)).toBe(expected);
});
