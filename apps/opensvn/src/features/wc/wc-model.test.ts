import { expect, test } from "vitest";
import type { FileStatus } from "../../ipc/bindings";
import { isSafeRelativePath } from "./dialogs/file-dialogs";
import { matchShortcut, shortcutLabel } from "./shortcuts";
import {
  buildRows,
  changedFiles,
  checkCommit,
  extensionPattern,
  fileGlyph,
  filterFiles,
  isCheckoutUrl,
  joinMessage,
  loadMessageHistory,
  lockFiles,
  pushMessageHistory,
  splitMessage,
  suggestMoves,
  tickedByDefault,
  tristate,
  wcPathUrl,
} from "./wc-model";

function f(path: string, patch: Partial<FileStatus> = {}): FileStatus {
  return {
    path,
    kind: "file",
    text: "modified",
    props: "none",
    treeConflict: false,
    movedFrom: null,
    movedTo: null,
    changelist: null,
    lock: null,
    needsLock: false,
    remote: null,
    binary: false,
    size: 1,
    ...patch,
  };
}

test("glyphs: tree and property conflicts show C, property-only changes show P, normal hides", () => {
  expect(fileGlyph(f("a", { treeConflict: true, text: "normal" }))).toBe("C");
  expect(fileGlyph(f("a", { text: "normal", props: "conflicted" }))).toBe("C");
  expect(fileGlyph(f("a", { text: "normal", props: "modified" }))).toBe("P");
  expect(fileGlyph(f("a", { text: "normal" }))).toBeNull();
  expect(changedFiles([f("a"), f("b", { text: "normal" })]).map((x) => x.path)).toEqual(["a"]);
});

test("unversioned and conflicted files are not ticked by default", () => {
  expect(tickedByDefault(f("a"))).toBe(true);
  expect(tickedByDefault(f("a", { text: "unversioned" }))).toBe(false);
  expect(tickedByDefault(f("a", { text: "conflicted" }))).toBe(false);
  expect(tickedByDefault(f("a", { text: "missing" }))).toBe(true);
});

test("filter combines text and OR-ed chips", () => {
  const files = [f("src/a.ts"), f("src/b.ts", { text: "unversioned" }), f("docs/c.md")];
  expect(filterFiles(files, "src", new Set()).length).toBe(2);
  expect(filterFiles(files, "", new Set(["unversioned" as const])).map((x) => x.path)).toEqual([
    "src/b.ts",
  ]);
  expect(
    filterFiles(files, "src", new Set(["unversioned" as const, "modified" as const])).length,
  ).toBe(2);
});

test("rows are grouped by changelist only when one exists, unassigned first", () => {
  expect(buildRows([f("a"), f("b")]).every((r) => r.kind === "file")).toBe(true);
  const rows = buildRows([f("a", { changelist: "zed" }), f("b"), f("c", { changelist: "alpha" })]);
  const heads = rows
    .filter((r) => r.kind === "group")
    .map((r) => (r.kind === "group" ? r.name : ""));
  expect(heads).toEqual([null, "alpha", "zed"]);
});

test("tristate reflects how many paths are ticked", () => {
  const c = new Set(["a"]);
  expect(tristate(["a", "b"], c)).toBe("some");
  expect(tristate(["a"], c)).toBe("all");
  expect(tristate(["b"], c)).toBe("none");
});

test("a missing file and an unversioned file with the same name suggest a move", () => {
  const files = [f("docs/x.md", { text: "missing" }), f("tools/x.md", { text: "unversioned" })];
  expect(suggestMoves(files)).toEqual([{ from: "docs/x.md", to: "tools/x.md" }]);
  expect(suggestMoves([f("docs/x.md", { text: "missing" })])).toEqual([]);
});

test("commit check flags conflicts, missing locks, foreign locks and additions", () => {
  const mine = { owner: "me", comment: null, createdAt: "", token: "t", mine: true };
  const other = { ...mine, owner: "an", mine: false };
  const c = checkCommit([
    f("a", { text: "conflicted" }),
    f("b", { needsLock: true }),
    f("c", { needsLock: true, lock: mine }),
    f("d", { lock: other }),
    f("e", { text: "unversioned" }),
  ]);
  expect(c.conflicts).toEqual(["a"]);
  expect(c.needsLock).toEqual(["b"]);
  expect(c.lockedByOther).toEqual(["d"]);
  expect(c.willAdd).toBe(1);
});

test("messages join and split symmetrically", () => {
  expect(joinMessage(" Fix ", " why \n")).toBe("Fix\n\nwhy");
  expect(splitMessage("Fix\n\nwhy")).toEqual({ summary: "Fix", description: "why" });
  expect(splitMessage("Only")).toEqual({ summary: "Only", description: "" });
});

function memoryStorage(initial?: string) {
  let v = initial ?? null;
  return {
    getItem: () => v,
    setItem: (_k: string, val: string) => {
      v = val;
    },
  };
}

test("message history is newest first, de-duplicated, capped and survives corrupt storage", () => {
  const s = memoryStorage();
  pushMessageHistory("one", s);
  pushMessageHistory("two", s);
  expect(pushMessageHistory("one", s)).toEqual(["one", "two"]);
  for (let i = 0; i < 40; i++) pushMessageHistory(`m${i}`, s);
  expect(loadMessageHistory(s).length).toBe(20);
  expect(loadMessageHistory(memoryStorage("{not json"))).toEqual([]);
  expect(loadMessageHistory(memoryStorage('[1,"ok",""]'))).toEqual(["ok"]);
  expect(loadMessageHistory(null)).toEqual([]);
});

test("helpers: extension pattern, checkout url, wc path url, safe relative path", () => {
  expect(extensionPattern("src/a.test.ts")).toBe("*.ts");
  expect(extensionPattern("Makefile")).toBeNull();
  expect(extensionPattern(".hidden")).toBeNull();
  expect(isCheckoutUrl("https://svn.example.com/r")).toBe(true);
  expect(isCheckoutUrl("javascript:alert(1)")).toBe(false);
  expect(isCheckoutUrl("ftp://x/y")).toBe(false);
  expect(wcPathUrl("https://h/r/trunk/", "a b/c#d.ts")).toBe("https://h/r/trunk/a%20b/c%23d.ts");
  expect(isSafeRelativePath("a/b.ts")).toBe(true);
  for (const bad of ["", "/etc/passwd", "../x", "a/../../x", "C:\\x", "\\\\srv\\x"]) {
    expect(isSafeRelativePath(bad)).toBe(false);
  }
});

function key(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent("keydown", init);
}

test("shortcuts need Mod and exact modifiers; Alt chords match on the physical key", () => {
  expect(matchShortcut(key({ key: "u", ctrlKey: true, shiftKey: true }), "update", false)).toBe(
    true,
  );
  expect(matchShortcut(key({ key: "u", ctrlKey: true }), "update", false)).toBe(false);
  expect(matchShortcut(key({ key: "U", metaKey: true, shiftKey: true }), "update", true)).toBe(
    true,
  );
  expect(matchShortcut(key({ key: "u", metaKey: true, shiftKey: true }), "update", false)).toBe(
    false,
  );
  expect(matchShortcut(key({ key: "l", ctrlKey: true }), "lock", false)).toBe(true);
  expect(matchShortcut(key({ key: "l", ctrlKey: true, shiftKey: true }), "lock", false)).toBe(
    false,
  );
  expect(
    matchShortcut(
      key({ key: "∂", code: "KeyD", metaKey: true, altKey: true }),
      "toggleDiffMode",
      true,
    ),
  ).toBe(true);
  expect(shortcutLabel("update", false)).toBe("Ctrl+Shift+U");
  expect(shortcutLabel("commit", true)).toBe("⌘Enter");
});

test("lockFiles: unmodified needs-lock or locked files, never changes or directories", () => {
  const mine = { owner: "you", comment: null, createdAt: "", token: null, mine: true };
  const files = [
    f("a.psd", { text: "normal", needsLock: true }),
    f("b.docx", { text: "normal", lock: mine }),
    f("c.txt", { text: "normal" }),
    f("d.psd", { text: "modified", needsLock: true }),
    f("e", { kind: "dir", text: "normal", lock: mine }),
    f("f.psd", { text: "normal", props: "modified", needsLock: true }),
  ];
  expect(lockFiles(files).map((x) => x.path)).toEqual(["a.psd", "b.docx"]);
  // Disjoint from the Changes list, so they are never ticked for a commit.
  const changed = new Set(changedFiles(files).map((x) => x.path));
  expect(lockFiles(files).some((x) => changed.has(x.path))).toBe(false);
});
