/** Sample data for the mock adapter (mirrors docs/system-design/ui-mockup-main-window.html; no real names). */
import type { FileStatus, LogEntry, NodeStatus, Settings, WorkingCopy } from "../bindings";

export const MOCK_WC_ID = "wc-main";

export const mockWorkingCopies: WorkingCopy[] = [
  {
    id: MOCK_WC_ID,
    name: "game-client",
    rootPath: "/Users/dev/work/game-client",
    repoRootUrl: "https://svn.example.com/repos/game",
    repoUuid: "00000000-0000-0000-0000-000000000001",
    url: "https://svn.example.com/repos/game/trunk",
    baseRev: 48210,
    revRange: null,
    format: 32,
    storePristine: true,
    branch: { kind: "trunk", name: "trunk", url: "https://svn.example.com/repos/game/trunk" },
    pathInRepo: "/trunk",
    incoming: 3,
    lastCheckedAt: "2026-10-13T09:00:00Z",
  },
  {
    id: "wc-docs",
    name: "design-docs",
    rootPath: "/Users/dev/work/design-docs",
    repoRootUrl: "https://svn.example.com/repos/docs",
    repoUuid: "00000000-0000-0000-0000-000000000002",
    url: "https://svn.example.com/repos/docs",
    baseRev: 912,
    revRange: null,
    format: 32,
    storePristine: true,
    branch: null,
    pathInRepo: "/",
    incoming: 0,
    lastCheckedAt: null,
  },
];

function fs(path: string, text: NodeStatus, extra: Partial<FileStatus> = {}): FileStatus {
  return {
    path,
    kind: "file",
    text,
    props: "none",
    treeConflict: false,
    movedFrom: null,
    movedTo: null,
    changelist: null,
    lock: null,
    needsLock: false,
    remote: null,
    binary: false,
    size: 2048,
    ...extra,
  };
}

export const mockStatus: FileStatus[] = [
  fs("src/engine/renderer.ts", "modified"),
  fs("src/engine/scene-graph.ts", "modified"),
  fs("src/ui/menu/main-menu.tsx", "added"),
  fs("assets/textures/hero-diffuse.png", "modified", {
    binary: true,
    needsLock: true,
    lock: {
      owner: "you",
      comment: null,
      createdAt: "2026-10-13T08:00:00Z",
      token: "t1",
      mine: true,
    },
  }),
  fs("assets/models/crate.fbx", "modified", {
    binary: true,
    lock: {
      owner: "an",
      comment: "rework",
      createdAt: "2026-10-12T08:00:00Z",
      token: null,
      mine: false,
    },
  }),
  fs("src/legacy/old-loader.ts", "deleted"),
  fs("src/engine/physics.ts", "conflicted"),
  fs("src/engine/audio.ts", "replaced"),
  fs("notes/todo.txt", "unversioned"),
  fs("src/engine/missing-file.ts", "missing"),
];

export const mockLog: LogEntry[] = Array.from({ length: 40 }, (_, i) => ({
  rev: 48213 - i,
  author: ["binh", "chi", "dung", "an"][i % 4] ?? "an",
  date: new Date(Date.UTC(2026, 9, 13, 9, 0) - i * 3_600_000 * 5).toISOString(),
  message:
    i === 0
      ? "Fix shadow cascade flicker\n\nDetails on second line."
      : `Update module ${i}: tweak and cleanup`,
  changedPaths: null,
  hasChildren: null,
}));

export const mockSettings: Settings = {
  "general.checkoutRoot": "/Users/dev/work",
  "general.checkIntervalMin": 5,
  "general.language": "vi",
  "appearance.theme": "system",
  "diff.mode": "split",
  "diff.ignoreWhitespace": false,
  "network.maxConnections": 4,
  "network.timeoutSec": 60,
  "network.proxy": { mode: "system" },
  "network.negotiate": true,
  "wc.defaultFormat": 32,
  "wc.readSubversionConfig": false,
  "cache.maxSizeGb": 2,
  "integrations.webRevisionUrl": "",
  "integrations.webFileUrl": "",
  "diff.externalTool": null,
  "diff.mergeTool": null,
  "tools.editor": null,
  "tools.terminal": null,
};
