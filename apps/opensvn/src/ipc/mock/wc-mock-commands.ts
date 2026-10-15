/**
 * Working-copy commands of the mock adapter: an in-memory working copy whose status reacts to
 * add / delete / move / revert / commit / update / resolve / lock, so `pnpm dev:mock` demonstrates
 * every working-copy flow. State lives in this module; `resetWcMock()` restores the sample data.
 */
import type {
  AppError,
  CheckoutRequest,
  CommitRequest,
  DiffGetRequest,
  DiffTarget,
  FileStatus,
  IgnoreAddRequest,
  LockConflict,
  LockInfo,
  LockRequest,
  OperationHandle,
  OperationKind,
  OpResult,
  PickRequest,
  ResolveRequest,
  RevertRequest,
  SparseEntry,
  UnlockRequest,
  UpdateRequest,
  WcEntry,
  WorkingCopy,
} from "../bindings";
import { MOCK_WC_ID, mockSettings, mockWorkingCopies } from "./data";
import { MOCK_REPO_ROOT, MOCK_YOUNGEST, mockRevDiff } from "./remote-data";
import { freshMainStatus, mockWcDiff } from "./wc-mock-fixtures";

type Reply<T> = Promise<{ status: "ok"; data: T } | { status: "error"; error: AppError }>;

/** Helpers the adapter shares with this module (kept as parameters to avoid an import cycle). */
export interface MockDeps {
  ok: <T>(data: T) => Reply<T>;
  fail: (code: AppError["code"], message: string, detail?: string | null) => Reply<never>;
  simulateOp: (
    kind: OperationKind,
    title: string,
    wcId: string | null,
    result?: OpResult,
    onDone?: () => void,
    failure?: AppError,
  ) => OperationHandle;
  emit: (name: string, payload?: unknown) => void;
}

interface State {
  wcs: WorkingCopy[];
  /** Registered working copies whose folder is gone (shown with "Locate…"). */
  missing: WcEntry[];
  lastOpened: Map<string, string>;
  recentUrls: string[];
  /** Recorded folder depths per working copy (`wc_depths`), the root first. */
  depths: Map<string, SparseEntry[]>;
  status: Map<string, FileStatus[]>;
  grants: Map<string, string>;
  youngest: number;
  seq: number;
  /**
   * Locks other users hold on files of the main working copy that its status does not show (as
   * after a plain local status): only a lock attempt reveals them.
   */
  serverLocks: Map<string, LockInfo>;
}

/** A registered working copy whose folder was moved, for the repository manager demo. */
export const MOCK_MISSING_WC: WcEntry = {
  id: "wc-missing",
  name: "old-prototype",
  rootPath: "/Users/dev/work/old-prototype",
  repoRootUrl: "https://svn.example.com/repos/game",
  repoUuid: "00000000-0000-0000-0000-000000000001",
  url: null,
  addedAt: "2026-05-02T10:00:00Z",
  lastOpenedAt: "2026-09-30T16:20:00Z",
  missing: true,
  problem: "/Users/dev/work/old-prototype is not a working copy",
  incomplete: false,
};

function initialState(): State {
  return {
    wcs: structuredClone(mockWorkingCopies),
    missing: [structuredClone(MOCK_MISSING_WC)],
    lastOpened: new Map([[MOCK_WC_ID, "2026-10-13T08:30:00Z"]]),
    recentUrls: ["https://svn.example.com/repos/game/trunk", "https://svn.example.com/repos/docs"],
    depths: new Map([
      [
        MOCK_WC_ID,
        [
          { path: "", depth: "infinity" },
          { path: "assets", depth: "immediates" },
          { path: "src", depth: "infinity" },
        ],
      ],
    ]),
    status: new Map([[MOCK_WC_ID, freshMainStatus()]]),
    grants: new Map(),
    youngest: MOCK_YOUNGEST,
    seq: 0,
    serverLocks: new Map([
      [
        "assets/audio/theme.ogg",
        {
          owner: "binh",
          comment: "mixing the final cut",
          createdAt: "2026-10-13T09:00:00Z",
          token: null,
          mine: false,
        },
      ],
    ]),
  };
}

function entryOf(w: WorkingCopy): WcEntry {
  return {
    id: w.id,
    name: w.name,
    rootPath: w.rootPath,
    repoRootUrl: w.repoRootUrl,
    repoUuid: w.repoUuid,
    url: w.url,
    addedAt: "2026-01-01T00:00:00Z",
    lastOpenedAt: state.lastOpened.get(w.id) ?? null,
    missing: false,
    problem: null,
    incomplete: w.incomplete,
    resume: w.resume ?? null,
  };
}

/**
 * Folder names the mock backend refuses although the dialog's own check accepts them (the backend
 * is the authority; this simulates a rule the front end does not mirror).
 */
const BACKEND_REFUSED_NAMES = new Set(["refused-by-backend"]);

/** Folder names the mock treats as existing, non-empty folders in every parent. */
const OCCUPIED_NAMES = new Set(["game-client", "design-docs"]);

let state = initialState();

/** Restores the sample working copies and status (tests, and dev reloads). */
export function resetWcMock(): void {
  state = initialState();
}

/** Empties the working copy list, as on a fresh install (tests: the Welcome screen). */
export function clearMockWorkingCopies(): void {
  state.wcs = [];
}

/**
 * Marks a sample working copy as left behind by a checkout (or, with `checkout` false, an update)
 * that stopped half-way at `rev` (tests).
 */
export function markMockWcIncomplete(wcId: string, rev = 1200, checkout = true): void {
  const wc = state.wcs.find((w) => w.id === wcId);
  if (wc) {
    wc.incomplete = true;
    wc.resume = { rev, checkout };
  }
}

const FOLDER_NAMES = ["game-assets", "tools-sandbox", "level-editor", "audio-bank"];
const baseName = (p: string) => p.slice(p.lastIndexOf("/") + 1);

function wcOf(wcId: string): WorkingCopy | undefined {
  return state.wcs.find((w) => w.id === wcId);
}
function patchWc(wcId: string, patch: Partial<WorkingCopy>) {
  state.wcs = state.wcs.map((w) => (w.id === wcId ? { ...w, ...patch } : w));
}
function statusOf(wcId: string): FileStatus[] {
  return state.status.get(wcId) ?? [];
}
/** Replaces the status list immutably (so query results change identity) and announces it. */
function setStatus(deps: MockDeps, wcId: string, next: FileStatus[]) {
  state.status.set(wcId, next);
  deps.emit("wc://status-changed", { wcId, changed: [], removed: [] });
}
function mapPaths(wcId: string, paths: string[], fn: (f: FileStatus) => FileStatus | null) {
  return statusOf(wcId).flatMap((f) => (paths.includes(f.path) ? (fn(f) ?? []) : [f]));
}
function globToRegExp(glob: string): RegExp {
  const esc = glob
    .replace(/[.+^${}()|[\]\\]/g, "\\$&")
    .replace(/\*/g, ".*")
    .replace(/\?/g, ".");
  return new RegExp(`^${esc}$`);
}
function consumeGrant(id: string): string | null {
  const name = state.grants.get(id) ?? null;
  state.grants.delete(id); // single use, like the real thing
  return name;
}
const BAD_GRANT = "The chosen folder is unknown, expired or was already used.";

export function wcMockCommands(deps: MockDeps) {
  const { ok, fail, simulateOp } = deps;
  const forWc = (wcId: string) => wcOf(wcId);

  /** Runs `change` when the simulated operation completes, like the engine would. */
  const op = (
    kind: OperationKind,
    title: string,
    wcId: string,
    change: () => void,
    result?: OpResult,
  ) => ok(simulateOp(kind, title, wcId, result, change));

  return {
    wcAddExisting: (grant: string) => {
      const name = consumeGrant(grant);
      if (!name) return fail("invalid-argument", BAD_GRANT);
      const wc: WorkingCopy = {
        ...(structuredClone(mockWorkingCopies[1]) as WorkingCopy),
        id: `wc-added-${++state.seq}`,
        name,
        rootPath: `/Users/dev/work/${name}`,
        incoming: 0,
      };
      state.wcs = [...state.wcs, wc];
      state.status.set(wc.id, []);
      return ok(wc);
    },
    pathPick: (req?: PickRequest) => {
      // A file dialog pretends the user chose a sample file (or kept the suggested name).
      if (req?.kind === "file" || req?.kind === "saveFile") {
        const displayName = req.kind === "saveFile" ? req.suggestedName : "release-notes.txt";
        const id = `grant-${++state.seq}`;
        state.grants.set(id, displayName);
        return ok({ id, displayName, kind: "file" as const });
      }
      const displayName = FOLDER_NAMES[state.seq++ % FOLDER_NAMES.length] as string;
      const id = `grant-${state.seq}`;
      state.grants.set(id, displayName);
      return ok({ id, displayName, kind: "dir" as const });
    },
    repoSave: (url: string, _rev: number | null, grant: string) =>
      consumeGrant(grant)
        ? ok(simulateOp("save", `Save ${baseName(url)}`, null))
        : fail("invalid-argument", BAD_GRANT),
    wcList: () => ok([...state.wcs]),
    wcEntries: () => ok([...state.wcs.map(entryOf), ...state.missing]),
    wcOpen: (wcId: string) => {
      const wc = forWc(wcId);
      if (!wc) return fail("not-a-working-copy", "Unknown working copy");
      state.lastOpened.set(wcId, new Date().toISOString());
      return ok(wc);
    },
    wcClose: (wcId: string) =>
      forWc(wcId) ? ok(null) : fail("not-a-working-copy", "Unknown working copy"),
    wcRename: (wcId: string, name: string) => {
      const n = name.trim();
      if (!n || n.length > 200)
        return fail("invalid-argument", "The name must be 1 to 200 characters.");
      if (!forWc(wcId) && !state.missing.some((m) => m.id === wcId)) {
        return fail("not-a-working-copy", "This working copy is not in the list.");
      }
      patchWc(wcId, { name: n });
      state.missing = state.missing.map((m) => (m.id === wcId ? { ...m, name: n } : m));
      return ok(null);
    },
    wcRemove: (wcId: string) => {
      const before = state.wcs.length + state.missing.length;
      state.wcs = state.wcs.filter((w) => w.id !== wcId);
      state.missing = state.missing.filter((m) => m.id !== wcId);
      state.status.delete(wcId);
      return state.wcs.length + state.missing.length < before
        ? ok(null)
        : fail("not-a-working-copy", "This working copy is not in the list.");
    },
    wcLocate: (wcId: string, grant: string) => {
      const entry = state.missing.find((m) => m.id === wcId);
      if (!entry) return fail("not-a-working-copy", "This working copy is not in the list.");
      const folder = consumeGrant(grant);
      if (!folder) return fail("invalid-argument", BAD_GRANT);
      const wc: WorkingCopy = {
        ...(structuredClone(mockWorkingCopies[0]) as WorkingCopy),
        id: entry.id,
        name: entry.name,
        rootPath: `/Users/dev/work/${folder}`,
        incoming: 0,
      };
      state.missing = state.missing.filter((m) => m.id !== wcId);
      state.wcs = [...state.wcs, wc];
      state.status.set(wc.id, []);
      state.lastOpened.set(wc.id, new Date().toISOString());
      return ok(wc);
    },
    revealInOs: (wcId: string) =>
      forWc(wcId) ? ok(null) : fail("invalid-argument", "The folder no longer exists."),
    recentUrls: () => ok([...state.recentUrls]),
    recentUrlRemove: (url: string) => {
      state.recentUrls = state.recentUrls.filter((u) => u !== url);
      return ok([...state.recentUrls]);
    },
    wcDepths: (wcId: string) =>
      forWc(wcId)
        ? ok([...(state.depths.get(wcId) ?? [{ path: "", depth: "infinity" as const }])])
        : fail("not-a-working-copy", "Unknown working copy"),
    wcSparseApply: (wcId: string, entries: SparseEntry[]) => {
      if (!forWc(wcId)) return fail("not-a-working-copy", "Unknown working copy");
      if (entries.some((e) => e.path === "" && e.depth === "exclude")) {
        return fail("wc-invalid-operation", "the working copy root cannot be excluded");
      }
      return op("set-depth", "Choose folders", wcId, () => {
        // Like the engine: each folder in turn, a folder's own setting resetting those below it.
        let next = [...(state.depths.get(wcId) ?? [{ path: "", depth: "infinity" as const }])];
        for (const e of entries) {
          const below = (p: string) => (e.path === "" ? p !== "" : p.startsWith(`${e.path}/`));
          next = next.filter((d) => d.path !== e.path && !below(d.path));
          next.push(e);
        }
        if (!next.some((d) => d.path === "")) next.push({ path: "", depth: "infinity" });
        next.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
        state.depths.set(wcId, next);
      });
    },
    checkoutRootGrant: () => {
      const id = `grant-root-${++state.seq}`;
      state.grants.set(id, baseName(mockSettings["general.checkoutRoot"]));
      return ok({
        id,
        displayName: baseName(mockSettings["general.checkoutRoot"]),
        kind: "dir" as const,
      });
    },
    statusGet: (req: { wcId: string }) => ok([...statusOf(req.wcId)]),
    statusRefresh: (wcId: string) => ok(simulateOp("status-refresh", "Refresh status", wcId)),
    statusRemote: (wcId: string) =>
      op("status-remote", "Check for updates", wcId, () =>
        deps.emit("wc://incoming", {
          wcId,
          count: wcOf(wcId)?.incoming ?? 0,
          latestRev: state.youngest,
        }),
      ),
    checkout: (req: CheckoutRequest) => {
      const name = req.destName;
      if (
        name !== null &&
        (!name ||
          /[\\/]/.test(name) ||
          name === "." ||
          name === ".." ||
          BACKEND_REFUSED_NAMES.has(name))
      ) {
        // Like the backend: checked before the grant is redeemed; `detail` is the refused name.
        return fail("invalid-argument", "Invalid folder name: the name is not allowed.", name);
      }
      const parent = consumeGrant(req.dest);
      if (!parent) return fail("invalid-argument", BAD_GRANT);
      const folder = name ?? parent;
      // Like the backend: the folder holds an interrupted checkout of this URL.
      const unfinished = state.wcs.find(
        (w) => baseName(w.rootPath) === folder && w.resume?.checkout && w.url === req.url,
      );
      if (unfinished?.resume) {
        return Promise.resolve({
          status: "error" as const,
          error: {
            code: "invalid-argument",
            message:
              "The checkout folder already exists and is not empty. Choose another name or an empty folder.",
            detail: null,
            path: unfinished.rootPath,
            retryable: false,
            actions: ["continue-checkout"],
            incompleteCheckout: {
              wcId: unfinished.id,
              rev: unfinished.resume.rev,
              rootPath: unfinished.rootPath,
            },
          } satisfies AppError,
        });
      }
      if (OCCUPIED_NAMES.has(folder)) {
        return fail(
          "invalid-argument",
          "The checkout folder already exists and is not empty. Choose another name or an empty folder.",
        );
      }
      state.recentUrls = [req.url, ...state.recentUrls.filter((u) => u !== req.url)];
      const wcId = `wc-checkout-${++state.seq}`;
      const newRev = req.rev ?? state.youngest;
      const handle = simulateOp(
        "checkout",
        `Checkout ${req.url}`,
        null,
        { kind: "checkedOut", wcId, newRev },
        () => {
          const wc: WorkingCopy = {
            ...(structuredClone(mockWorkingCopies[0]) as WorkingCopy),
            id: wcId,
            name: folder,
            rootPath: `/Users/dev/work/${folder}`,
            url: req.url,
            baseRev: newRev,
            incoming: 0,
            branch: null,
          };
          state.wcs = [...state.wcs, wc];
          state.status.set(wc.id, []);
          deps.emit("wc://incoming", { wcId: wc.id, count: 0, latestRev: state.youngest });
        },
      );
      return ok(handle);
    },
    diffGet: (req: DiffGetRequest) =>
      ok(
        req.target.kind === "wc"
          ? mockWcDiff(req.target.path, req.forceLarge === true)
          : mockRevDiff("/trunk/src/engine/renderer.ts"),
      ),
    diffExternal: (target: DiffTarget) => {
      if (!mockSettings["diff.externalTool"]) {
        return Promise.resolve({
          status: "error" as const,
          error: {
            code: "invalid-argument",
            message: "No external diff tool is configured. Choose one in Settings.",
            detail: null,
            path: "path" in target ? target.path : null,
            retryable: false,
            actions: ["open-settings"],
          } satisfies AppError,
        });
      }
      return deps.ok(null);
    },
    diffOpenVersion: () => ok(null),
    changelistSet: (wcId: string, paths: string[], name: string | null) => {
      setStatus(
        deps,
        wcId,
        mapPaths(wcId, paths, (f) => ({ ...f, changelist: name })),
      );
      return ok(null);
    },
    ignoreAdd: (req: IgnoreAddRequest) => {
      const re = globToRegExp(req.pattern);
      const hidden = (f: FileStatus) =>
        f.text === "unversioned" &&
        (req.dir === "" || f.path === req.dir || f.path.startsWith(`${req.dir}/`)) &&
        (re.test(baseName(f.path)) || f.path.split("/").some((seg) => re.test(seg)));
      setStatus(
        deps,
        req.wcId,
        statusOf(req.wcId).filter((f) => !hidden(f)),
      );
      return ok(null);
    },
    markMove: (wcId: string, from: string, to: string) => {
      setStatus(deps, wcId, moved(statusOf(wcId), from, to));
      return ok(null);
    },
    addPaths: (wcId: string, paths: string[]) =>
      op("add", `Add ${paths.length} file(s)`, wcId, () =>
        setStatus(
          deps,
          wcId,
          mapPaths(wcId, paths, (f) => ({ ...f, text: "added" })),
        ),
      ),
    deletePaths: (wcId: string, paths: string[]) =>
      op("delete", `Delete ${paths.length} file(s)`, wcId, () =>
        setStatus(
          deps,
          wcId,
          mapPaths(wcId, paths, (f) =>
            f.text === "unversioned" || f.text === "added" ? null : { ...f, text: "deleted" },
          ),
        ),
      ),
    movePath: (wcId: string, from: string, to: string) =>
      op("move", `Move ${baseName(from)}`, wcId, () =>
        setStatus(deps, wcId, moved(statusOf(wcId), from, to)),
      ),
    revert: (req: RevertRequest) =>
      op("revert", `Discard ${req.paths.length} file(s)`, req.wcId, () =>
        setStatus(
          deps,
          req.wcId,
          mapPaths(req.wcId, req.paths, (f) =>
            f.text === "unversioned"
              ? req.removeUnversioned
                ? null
                : f
              : f.text === "added"
                ? { ...f, text: "unversioned" }
                : null,
          ),
        ),
      ),
    resolve: (req: ResolveRequest) => {
      setStatus(
        deps,
        req.wcId,
        mapPaths(req.wcId, [req.path], (f) =>
          req.choice === "theirs" || req.choice === "base" || f.treeConflict
            ? null
            : { ...f, text: "modified", props: "none", treeConflict: false },
        ),
      );
      return ok(null);
    },
    lock: (req: LockRequest) => {
      const { wcId, paths } = req.targets as { wcId: string; paths: string[] };
      // Like the real engine, a lock held elsewhere is only known once the server refuses the
      // lock: the operation fails with the refusing locks (status may not show them).
      const conflicts: LockConflict[] = paths.flatMap((path) => {
        const known = statusOf(wcId).find((f) => f.path === path && f.lock && !f.lock.mine)?.lock;
        const held = known ?? state.serverLocks.get(path);
        return held
          ? [{ path, owner: held.owner, comment: held.comment, createdAt: held.createdAt }]
          : [];
      });
      const title = `Lock ${paths.length} file(s)`;
      if (conflicts.length > 0 && !req.steal) {
        const owner = conflicts[0]?.owner ?? "another user";
        return ok(
          deps.simulateOp("lock", title, wcId, undefined, undefined, {
            code: "locked-by-other",
            message: `HTTP 423: locked by ${owner}`,
            detail: null,
            path: null,
            retryable: false,
            actions: ["open-lock-dialog"],
            lockConflicts: conflicts,
          }),
        );
      }
      return op("lock", title, wcId, () => {
        for (const p of paths) state.serverLocks.delete(p);
        setStatus(
          deps,
          wcId,
          mapPaths(wcId, paths, (f) => ({
            ...f,
            lock: {
              owner: "you",
              comment: req.comment,
              createdAt: new Date().toISOString(),
              token: null,
              mine: true,
            },
          })),
        );
      });
    },
    unlock: (req: UnlockRequest) => {
      const { wcId, paths } = req.targets as { wcId: string; paths: string[] };
      const others = statusOf(wcId).some((f) => paths.includes(f.path) && f.lock && !f.lock.mine);
      if (others && !req.breakLock) return fail("locked-by-other", "Locked by another user");
      return op("unlock", `Unlock ${paths.length} file(s)`, wcId, () =>
        setStatus(
          deps,
          wcId,
          mapPaths(wcId, paths, (f) => ({ ...f, lock: null })),
        ),
      );
    },
    commit: (req: CommitRequest) => {
      const files = statusOf(req.wcId).filter((f) => req.paths.includes(f.path));
      if (req.message.trim() === "") return fail("invalid-argument", "Commit message is empty");
      if (files.some((f) => f.text === "conflicted" || f.treeConflict)) {
        return fail("conflict-blocks-commit", "Resolve conflicts before committing");
      }
      if (files.some((f) => f.remote?.outOfDate)) {
        return fail("out-of-date", "File is out of date; update first");
      }
      const rev = state.youngest + 1;
      return op(
        "commit",
        `Commit ${req.paths.length} file(s)`,
        req.wcId,
        () => {
          state.youngest = rev;
          setStatus(
            deps,
            req.wcId,
            // Committed files are no longer changes; a kept lock stays on the (now normal) file,
            // and a needs-lock file stays listed (read-only again once its lock is released).
            mapPaths(req.wcId, req.paths, (f) => {
              const lock = req.keepLocks ? f.lock : null;
              return lock || f.needsLock ? { ...f, text: "normal", props: "none", lock } : null;
            }),
          );
          patchWc(req.wcId, { baseRev: rev });
          deps.emit("wc://revision-changed", { wcId: req.wcId, baseRev: rev, revRange: null });
        },
        { kind: "newRev", newRev: rev },
      );
    },
    wcContinue: (wcId: string) => {
      const wc = wcOf(wcId);
      if (!wc?.resume) {
        return fail(
          "wc-invalid-operation",
          "There is nothing to continue: the working copy is complete.",
        );
      }
      const target = wc.resume.rev;
      return op(
        "update",
        `Continue checkout ${wc.name}`,
        wcId,
        () => {
          patchWc(wcId, { baseRev: target, incomplete: false, resume: null });
          deps.emit("wc://revision-changed", { wcId, baseRev: target, revRange: null });
        },
        { kind: "newRev", newRev: target },
      );
    },
    update: (req: UpdateRequest) => {
      const target = req.rev ?? state.youngest;
      return op(
        "update",
        req.rev ? `Update to r${req.rev}` : "Update to HEAD",
        req.wcId,
        () => {
          setStatus(
            deps,
            req.wcId,
            statusOf(req.wcId).map((f) => (f.remote ? { ...f, remote: null } : f)),
          );
          // An update at any revision also finishes an interrupted checkout.
          patchWc(req.wcId, { baseRev: target, incoming: 0, incomplete: false, resume: null });
          deps.emit("wc://revision-changed", { wcId: req.wcId, baseRev: target, revRange: null });
        },
        { kind: "newRev", newRev: target },
      );
    },
    switch: (wcId: string, url: string, rev: number | null) =>
      op(
        "switch",
        `Switch to ${baseName(url)}`,
        wcId,
        () => {
          const seg = url.match(/\/(branches|tags)\/([^/]+)/);
          patchWc(wcId, {
            url,
            branch: seg
              ? { kind: seg[1] === "tags" ? "tag" : "branch", name: seg[2] as string, url }
              : url.endsWith("/trunk")
                ? { kind: "trunk", name: "trunk", url }
                : null,
            pathInRepo: url.slice(MOCK_REPO_ROOT.length) || "/",
          });
          deps.emit("wc://revision-changed", {
            wcId,
            baseRev: rev ?? state.youngest,
            revRange: null,
          });
        },
        { kind: "newRev", newRev: rev ?? state.youngest },
      ),
    cleanup: (req: { wcId: string }) => op("cleanup", "Clean up", req.wcId, () => {}),
  };
}

/** `from` becomes a scheduled delete (moved to `to`), `to` an addition copied from `from`. */
function moved(list: FileStatus[], from: string, to: string): FileStatus[] {
  const source = list.find((f) => f.path === from);
  const rest = list.filter((f) => f.path !== from && f.path !== to);
  return [
    ...rest,
    { ...(source ?? ({} as FileStatus)), path: from, text: "deleted", movedTo: to },
    { ...(source ?? ({} as FileStatus)), path: to, text: "added", movedFrom: from, movedTo: null },
  ];
}
