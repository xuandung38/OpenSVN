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
  FileStatus,
  IgnoreAddRequest,
  LockRequest,
  OperationHandle,
  OperationKind,
  OpResult,
  ResolveRequest,
  RevertRequest,
  UnlockRequest,
  UpdateRequest,
  WorkingCopy,
} from "../bindings";
import { MOCK_WC_ID, mockWorkingCopies } from "./data";
import { MOCK_REPO_ROOT, MOCK_YOUNGEST, mockRevDiff } from "./remote-data";
import { freshMainStatus, mockWcDiff } from "./wc-mock-fixtures";

type Reply<T> = Promise<{ status: "ok"; data: T } | { status: "error"; error: AppError }>;

/** Helpers the adapter shares with this module (kept as parameters to avoid an import cycle). */
export interface MockDeps {
  ok: <T>(data: T) => Reply<T>;
  fail: (code: AppError["code"], message: string) => Reply<never>;
  simulateOp: (
    kind: OperationKind,
    title: string,
    wcId: string | null,
    result?: OpResult,
    onDone?: () => void,
  ) => OperationHandle;
  emit: (name: string, payload?: unknown) => void;
}

interface State {
  wcs: WorkingCopy[];
  status: Map<string, FileStatus[]>;
  grants: Map<string, string>;
  youngest: number;
  seq: number;
}

function initialState(): State {
  return {
    wcs: structuredClone(mockWorkingCopies),
    status: new Map([[MOCK_WC_ID, freshMainStatus()]]),
    grants: new Map(),
    youngest: MOCK_YOUNGEST,
    seq: 0,
  };
}

let state = initialState();

/** Restores the sample working copies and status (tests, and dev reloads). */
export function resetWcMock(): void {
  state = initialState();
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
    pathPick: () => {
      const displayName = FOLDER_NAMES[state.seq++ % FOLDER_NAMES.length] as string;
      const id = `grant-${state.seq}`;
      state.grants.set(id, displayName);
      return ok({ id, displayName, kind: "dir" as const });
    },
    wcList: () => ok([...state.wcs]),
    wcOpen: (wcId: string) => {
      const wc = forWc(wcId);
      return wc ? ok(wc) : fail("not-a-working-copy", "Unknown working copy");
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
      const name = consumeGrant(req.dest);
      if (!name) return fail("invalid-argument", BAD_GRANT);
      const handle = simulateOp("checkout", `Checkout ${req.url}`, null, undefined, () => {
        const wc: WorkingCopy = {
          ...(structuredClone(mockWorkingCopies[0]) as WorkingCopy),
          id: `wc-checkout-${++state.seq}`,
          name,
          rootPath: `/Users/dev/work/${name}`,
          url: req.url,
          baseRev: req.rev ?? state.youngest,
          incoming: 0,
          branch: null,
        };
        state.wcs = [...state.wcs, wc];
        state.status.set(wc.id, []);
        deps.emit("wc://incoming", { wcId: wc.id, count: 0, latestRev: state.youngest });
      });
      return ok(handle);
    },
    diffGet: (req: DiffGetRequest) =>
      ok(
        req.target.kind === "wc"
          ? mockWcDiff(req.target.path, req.forceLarge === true)
          : mockRevDiff("/trunk/src/engine/renderer.ts"),
      ),
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
      const taken = statusOf(wcId).find((f) => paths.includes(f.path) && f.lock && !f.lock.mine);
      if (taken && !req.steal) {
        return fail("locked-by-other", `Locked by ${taken.lock?.owner ?? "another user"}`);
      }
      return op("lock", `Lock ${paths.length} file(s)`, wcId, () =>
        setStatus(
          deps,
          wcId,
          mapPaths(wcId, paths, (f) => ({
            ...f,
            lock: {
              owner: "you",
              comment: req.comment,
              createdAt: new Date().toISOString(),
              token: `tok-${f.path}`,
              mine: true,
            },
          })),
        ),
      );
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
            // Committed files are no longer changes; a kept lock stays on the (now normal) file.
            mapPaths(req.wcId, req.paths, (f) =>
              f.lock && req.keepLocks ? { ...f, text: "normal", props: "none" } : null,
            ),
          );
          patchWc(req.wcId, { baseRev: rev });
          deps.emit("wc://revision-changed", { wcId: req.wcId, baseRev: rev, revRange: null });
        },
        { kind: "newRev", newRev: rev },
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
          patchWc(req.wcId, { baseRev: target, incoming: 0 });
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
