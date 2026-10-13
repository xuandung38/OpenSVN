/** Sample repository for the mock adapter's remote commands (browse, log, blame, locks). */
import type {
  BlameLine,
  BranchNode,
  ChangedPath,
  FileDiff,
  LockInfo,
  LogEntry,
  RepoEntry,
} from "../bindings";
import { mockLog } from "./data";

export const MOCK_REPO_ROOT = "https://svn.example.com/repos/game";
export const MOCK_REPO_UUID = "00000000-0000-0000-0000-000000000001";
export const MOCK_YOUNGEST = 48213;

/** Hostile name on purpose: UI must render it as text (never as markup). */
export const HOSTILE_NAME = "<img src=x onerror=alert(1)>.txt";

const lockOther: LockInfo = {
  owner: "an",
  comment: "reworking the model",
  createdAt: "2026-10-12T08:00:00Z",
  token: null,
  mine: false,
};
const lockMine = (): LockInfo => ({
  owner: "you",
  comment: null,
  createdAt: "2026-10-13T08:00:00Z",
  token: "tok-1",
  mine: true,
});

/** URLs the current user has locked (mutated by mock lock/unlock). */
export const myLocks = new Set<string>();
/** URLs locked by someone else. */
export const otherLocks = new Set<string>([`${MOCK_REPO_ROOT}/trunk/assets/models/crate.fbx`]);

function entry(name: string, kind: "file" | "dir", extra: Partial<RepoEntry> = {}): RepoEntry {
  return {
    name,
    kind,
    size: kind === "file" ? 2048 + name.length * 37 : null,
    createdRev: 48000 + name.length,
    lastAuthor: ["binh", "chi", "dung", "an"][name.length % 4] ?? "an",
    date: "2026-10-10T09:00:00Z",
    lock: null,
    ...extra,
  };
}

const TREE: Record<string, RepoEntry[]> = {
  "": [entry("trunk", "dir"), entry("branches", "dir"), entry("tags", "dir")],
  trunk: [
    entry("src", "dir"),
    entry("assets", "dir"),
    entry("notes", "dir"),
    entry("README.md", "file"),
    entry("build.gradle", "file"),
  ],
  "trunk/src": [entry("engine", "dir"), entry("ui", "dir"), entry("main.ts", "file")],
  "trunk/src/engine": [
    entry("renderer.ts", "file"),
    entry("scene-graph.ts", "file"),
    entry("physics.ts", "file"),
  ],
  "trunk/src/ui": [entry("main-menu.tsx", "file")],
  "trunk/assets": [entry("textures", "dir"), entry("models", "dir")],
  "trunk/assets/textures": [entry("hero-diffuse.png", "file", { size: 482_000 })],
  "trunk/assets/models": [entry("crate.fbx", "file", { size: 1_240_000 })],
  "trunk/notes": [entry(HOSTILE_NAME, "file"), entry("todo.txt", "file")],
  branches: [entry("feature-x", "dir"), entry("release-1.0", "dir")],
  "branches/feature-x": [entry("README.md", "file")],
  "branches/release-1.0": [entry("README.md", "file")],
  tags: [entry("v1.0.0", "dir"), entry("v2.4.0", "dir")],
};

/** Directory listing for `url` (relative path resolved against the mock root). */
export function mockList(url: string): RepoEntry[] | null {
  if (!url.startsWith(MOCK_REPO_ROOT)) return null;
  const rel = url.slice(MOCK_REPO_ROOT.length).replace(/^\/+|\/+$/g, "");
  const entries = TREE[rel];
  if (!entries) return null;
  return entries.map((e) => {
    const child = `${MOCK_REPO_ROOT}/${rel ? `${rel}/` : ""}${e.name}`;
    const lock = myLocks.has(child) ? lockMine() : otherLocks.has(child) ? lockOther : null;
    return { ...e, lock };
  });
}

export function mockFileText(url: string): string {
  const name = decodeURIComponent(url.split("/").pop() ?? "file");
  const lines = [`// ${name}`, "export function render(frame: number): void {"];
  for (let i = 0; i < 24; i++) lines.push(`  draw(${i}, frame * ${i + 1}); // step ${i}`);
  lines.push("}");
  if (name === HOSTILE_NAME) lines.push("<script>window.__pwned = 1</script>");
  return lines.join("\n");
}

const PATHS: ChangedPath[][] = [
  [
    {
      path: "/trunk/src/engine/renderer.ts",
      action: "M",
      kind: "file",
      copyFrom: null,
      textMods: true,
      propMods: false,
    },
    {
      path: "/trunk/src/engine/shadow-cascade.ts",
      action: "A",
      kind: "file",
      copyFrom: null,
      textMods: true,
      propMods: false,
    },
  ],
  [
    {
      path: "/trunk/assets/textures/hero-diffuse.png",
      action: "M",
      kind: "file",
      copyFrom: null,
      textMods: true,
      propMods: false,
    },
  ],
  [
    {
      path: "/trunk/src/legacy/old-loader.ts",
      action: "D",
      kind: "file",
      copyFrom: null,
      textMods: null,
      propMods: null,
    },
    {
      path: "/trunk/src/engine/audio.ts",
      action: "R",
      kind: "file",
      copyFrom: null,
      textMods: true,
      propMods: false,
    },
  ],
  [
    {
      path: "/branches/feature-x",
      action: "A",
      kind: "dir",
      copyFrom: { path: "/trunk", rev: 48190 },
      textMods: null,
      propMods: null,
    },
  ],
];

export const mockLogWithPaths: LogEntry[] = mockLog.map((e, i) => ({
  ...e,
  message:
    i === 1
      ? "Add feature-x branch\n\nSee https://tracker.example.com/issue/42 and <script>alert(1)</script>"
      : i === 2
        ? `Rename asset ${HOSTILE_NAME}`
        : e.message,
  changedPaths: PATHS[i % PATHS.length] ?? null,
}));

export const mockBranches: BranchNode[] = [
  { kind: "trunk", name: "trunk", url: `${MOCK_REPO_ROOT}/trunk` },
  { kind: "branch", name: "feature-x", url: `${MOCK_REPO_ROOT}/branches/feature-x` },
  { kind: "branch", name: "release-1.0", url: `${MOCK_REPO_ROOT}/branches/release-1.0` },
  { kind: "tag", name: "v1.0.0", url: `${MOCK_REPO_ROOT}/tags/v1.0.0` },
  { kind: "tag", name: "v2.4.0", url: `${MOCK_REPO_ROOT}/tags/v2.4.0` },
];

export function mockBlame(): BlameLine[] {
  return mockFileText("renderer.ts")
    .split("\n")
    .map((text, i) => ({
      rev: 48213 - (i % 7) * 3,
      author: ["binh", "chi", "dung", "an"][i % 4] ?? null,
      date: new Date(Date.UTC(2026, 9, 13 - (i % 9))).toISOString(),
      text,
    }));
}

export function mockRevDiff(path: string): FileDiff {
  return {
    path,
    binary: false,
    mime: null,
    left: {
      content: "const a = 1;\nconst b = 2;\n",
      encoding: "UTF-8",
      eol: "LF",
      label: "r48212",
    },
    right: {
      content: "const a = 1;\nconst b = 3;\nconst c = 4;\n",
      encoding: "UTF-8",
      eol: "LF",
      label: "r48213",
    },
    hunks: null,
    propChanges: [],
    image: null,
    tooLarge: false,
  };
}
