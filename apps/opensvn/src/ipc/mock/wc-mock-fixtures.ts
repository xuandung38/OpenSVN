/** Working-copy fixtures for the mock adapter: per-file diffs and the extra demo files. */
import type { FileDiff, FileStatus, TextSide } from "../bindings";
import { mockStatus } from "./data";

const side = (content: string, label: string): TextSide => ({
  content,
  encoding: "UTF-8",
  eol: "LF",
  label,
});

const NO_DIFF: Omit<FileDiff, "path"> = {
  binary: false,
  mime: null,
  left: null,
  right: null,
  hunks: null,
  propChanges: [],
  image: null,
  tooLarge: null,
};

function status(path: string, patch: Partial<FileStatus>): FileStatus {
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
    size: 4096,
    ...patch,
  };
}

/** Lines `a-0..a-N` vs `b-0..b-N`: nothing in common, so the diff cost exceeds DISPLAY_MAX_COST. */
function disjointLines(prefix: string, n: number): string {
  return Array.from({ length: n }, (_, i) => `${prefix}-${i}`).join("\n");
}

/** A fresh copy of the demo status of the main working copy (plus files for every diff state). */
export function freshMainStatus(): FileStatus[] {
  const base = structuredClone(mockStatus).map((f) =>
    // The replaced file is newer on the server: committing it demonstrates the out-of-date flow.
    f.path === "src/engine/audio.ts"
      ? { ...f, remote: { outOfDate: true, remoteRev: 48213, remoteLock: null } }
      : f,
  );
  return [
    ...base,
    status("data/big-table.csv", {}),
    status("logs/huge-trace.log", { size: 7_000_000 }),
    status("docs/readme-renamed.md", { text: "missing" }),
    status("tools/readme-renamed.md", { text: "unversioned" }),
    status("art", { kind: "dir", text: "normal", treeConflict: true }),
  ];
}

const CONFLICT_TEXT = [
  "export class Physics {",
  "<<<<<<< .mine",
  "  gravity = 9.81;",
  "=======",
  "  gravity = 9.8;",
  ">>>>>>> .r48213",
  "}",
  "",
].join("\n");

/** Diff of one file against its base, shaped like the backend's `diff_get` answer. */
export function mockWcDiff(path: string, forceLarge: boolean): FileDiff {
  const name = path.split("/").pop() ?? path;
  const base = { ...NO_DIFF, path };
  switch (path) {
    case "assets/textures/hero-diffuse.png":
      return { ...base, binary: true, mime: "image/png" };
    case "assets/models/crate.fbx":
      return { ...base, binary: true, mime: "application/octet-stream" };
    case "data/big-table.csv":
      return {
        ...base,
        left: side(disjointLines("a", 6000), "base"),
        right: side(disjointLines("b", 6000), "working"),
      };
    case "logs/huge-trace.log":
      return forceLarge
        ? {
            ...base,
            left: side("start\nline\nend\n", "base"),
            right: side("start\nline changed\nend\n", "working"),
            tooLarge: true,
          }
        : {
            ...base,
            tooLarge: true,
            hunks: [
              {
                oldStart: 2,
                oldLines: 1,
                newStart: 2,
                newLines: 1,
                lines: ["-line", "+line changed"],
              },
            ],
          };
    case "src/engine/physics.ts":
      return {
        ...base,
        left: side("export class Physics {\n  gravity = 9.8;\n}\n", "base"),
        right: side(CONFLICT_TEXT, "working"),
      };
    case "src/engine/scene-graph.ts":
      return {
        ...base,
        left: side("export class SceneGraph {}\n", "base"),
        right: side("export class SceneGraph {\n  nodes = [];\n}\n", "working"),
        propChanges: [{ name: "svn:eol-style", before: null, after: "native" }],
      };
  }
  if (path === "art") {
    return { ...base, propChanges: [{ name: "svn:ignore", before: "obj", after: "obj\nbin" }] };
  }
  const added = mockStatus.find((f) => f.path === path)?.text;
  if (added === "added" || added === "unversioned" || path.startsWith("tools/")) {
    return { ...base, right: side(`// ${name}\nexport const created = true;\n`, "working") };
  }
  if (added === "deleted" || added === "missing" || path.startsWith("docs/")) {
    return { ...base, left: side(`// ${name}\nexport const removed = true;\n`, "base") };
  }
  return {
    ...base,
    left: side(`// ${name}\nconst a = 1;\nconst b = 2;\nexport { a, b };\n`, "base"),
    right: side(
      `// ${name}\nconst a = 1;\nconst b = 3;\nconst c = 4;\nexport { a, b, c };\n`,
      "working",
    ),
  };
}
