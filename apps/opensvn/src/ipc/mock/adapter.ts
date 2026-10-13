import type { Commands, IpcAdapter } from "../adapter";
import type {
  AppError,
  CopyLocationRequest,
  LockRequest,
  LogGetRequest,
  OperationHandle,
  OperationKind,
  OpResult,
  ProgramKind,
  RemoteEditRequest,
  Settings,
  SettingsPatch_Deserialize,
  UnlockRequest,
} from "../bindings";
import { mockSettings } from "./data";
import {
  MOCK_REPO_ROOT,
  MOCK_REPO_UUID,
  MOCK_YOUNGEST,
  mockBlame,
  mockBranches,
  mockFileText,
  mockList,
  mockLogWithPaths,
  myLocks,
  otherLocks,
} from "./remote-data";
import { wcMockCommands } from "./wc-mock-commands";

type Handler = (payload: unknown) => void;
const listeners = new Map<string, Set<Handler>>();

/** Emit an event to mock subscribers (also used by the dev gallery to simulate backend pushes). */
export function mockEmit(name: string, payload: unknown = null): void {
  for (const h of listeners.get(name) ?? []) h(payload);
}

const ok = <T>(data: T) => Promise.resolve({ status: "ok" as const, data });
const err = (message: string): Promise<{ status: "error"; error: AppError }> =>
  Promise.resolve({
    status: "error",
    error: { code: "internal", message, detail: null, path: null, retryable: false, actions: null },
  });

const errCode = (code: AppError["code"], message: string) =>
  Promise.resolve({
    status: "error" as const,
    error: {
      code,
      message,
      detail: null,
      path: null,
      retryable: false,
      actions: null,
    } satisfies AppError,
  });

let opSeq = 0;
let opDurationMs = 2000;
/** Length of a simulated operation (tests shorten it; the dev server keeps it visible). */
export function setMockOpDuration(ms: number): void {
  opDurationMs = ms;
}
/** Operations the user cancelled; their remaining timers do nothing. */
const cancelled = new Set<string>();

/**
 * Simulates a long-running operation: op://started, a few op://progress ticks, then `onDone` (the
 * state change the real engine would have made) and op://finished. `op_cancel` turns it into an
 * op://failed with the `cancelled` code instead.
 */
function simulateOp(
  kind: OperationKind,
  title: string,
  wcId: string | null,
  result: OpResult = { kind: "done" },
  onDone?: () => void,
): OperationHandle {
  const handle: OperationHandle = { opId: `mock-op-${++opSeq}`, kind, wcId, title };
  setTimeout(() => mockEmit("op://started", handle), 0);
  for (const pct of [25, 50, 75]) {
    setTimeout(
      () => {
        if (cancelled.has(handle.opId)) return;
        mockEmit("op://progress", {
          opId: handle.opId,
          phase: kind,
          current: pct,
          total: 100,
          bytesDone: null,
          bytesTotal: null,
          currentPath: null,
        });
      },
      (pct / 100) * opDurationMs * 0.9,
    );
  }
  setTimeout(() => {
    if (cancelled.has(handle.opId)) return;
    onDone?.();
    mockEmit("op://finished", { opId: handle.opId, result });
    if (wcId) mockEmit("wc://status-changed", { wcId, changed: [], removed: [] });
  }, opDurationMs);
  return handle;
}

/** Every command invoked on the mock adapter, in order (assertions in tests). */
export const mockCalls: { name: string; args: unknown[] }[] = [];

/** Answers sent through `auth_respond`, newest last (assertions in tests and dev demos). */
export const mockAuthResponses: { requestId: string; response: unknown }[] = [];

const {
  lock: wcLock,
  unlock: wcUnlock,
  ...wcCommands
} = wcMockCommands({
  ok,
  fail: errCode,
  simulateOp,
  emit: mockEmit,
});

const PROGRAM_KEYS = {
  "diff-tool": "diff.externalTool",
  "merge-tool": "diff.mergeTool",
  editor: "tools.editor",
  terminal: "tools.terminal",
} as const satisfies Record<ProgramKind, keyof Settings>;

const implemented: Partial<Record<keyof Commands, (...args: never[]) => unknown>> = {
  ...wcCommands,
  opCancel: (opId: string) => {
    cancelled.add(opId);
    mockEmit("op://failed", {
      opId,
      error: {
        code: "cancelled",
        message: "Cancelled",
        detail: null,
        path: null,
        retryable: false,
        actions: null,
      } satisfies AppError,
    });
    return ok(null);
  },
  settingsGet: () => ok(mockSettings),
  opList: () => ok([]),
  repoInfo: (url: string) =>
    ok({
      repoRootUrl: MOCK_REPO_ROOT,
      repoUuid: MOCK_REPO_UUID,
      youngestRev: MOCK_YOUNGEST,
      kind: "dir" as const,
      url,
    }),
  repoList: (url: string) => {
    const list = mockList(url);
    return list ? ok(list) : errCode("path-not-found", `Path not found: ${url}`);
  },
  repoCat: (url: string) =>
    ok({
      content: mockFileText(url),
      encoding: "UTF-8",
      eol: "LF" as const,
      label: url.split("/").pop() ?? "",
    }),
  logGet: (req: LogGetRequest) => {
    const all = mockLogWithPaths;
    const q = req.filter?.toLowerCase();
    const filtered = q
      ? all.filter((e) => `${e.message} ${e.author ?? ""}`.toLowerCase().includes(q))
      : all;
    const start =
      req.fromRev == null ? 0 : filtered.findIndex((e) => e.rev <= (req.fromRev as number));
    const page = start < 0 ? [] : filtered.slice(start, start + req.limit);
    const next = filtered[start + req.limit];
    return ok({
      entries: req.withPaths ? page : page.map((e) => ({ ...e, changedPaths: null })),
      nextRev: next ? next.rev : null,
    });
  },
  blameGet: () => ok(simulateOp("blame", "Blame", null, { kind: "blame", lines: mockBlame() })),
  lock: (req: LockRequest) => {
    if (req.targets.kind === "wc") return wcLock(req);
    if (req.targets.kind === "url") {
      for (const u of req.targets.urls) {
        if (otherLocks.has(u) && !req.steal) {
          return errCode("locked-by-other", "Locked by an: reworking the model");
        }
        otherLocks.delete(u);
        myLocks.add(u);
      }
    }
    return ok(simulateOp("lock", "Lock", null));
  },
  unlock: (req: UnlockRequest) => {
    if (req.targets.kind === "wc") return wcUnlock(req);
    if (req.targets.kind === "url") {
      for (const u of req.targets.urls) {
        myLocks.delete(u);
        if (req.breakLock) otherLocks.delete(u);
      }
    }
    return ok(simulateOp("unlock", "Unlock", null));
  },
  remoteEdit: (req: RemoteEditRequest) =>
    ok(simulateOp("remote-edit", req.message || "Remote edit", null)),
  copyLocation: (req: CopyLocationRequest) =>
    ok(simulateOp("remote-edit", `Copy to ${req.toUrl}`, null)),
  layoutGet: () => ok({ mode: "auto" as const }),
  branchesList: () => ok(mockBranches),
  accountsList: () =>
    ok([
      {
        realm: "<https://svn.example.com:443> Game repository",
        username: "dev",
        method: "basic" as const,
        lastUsedAt: "2026-10-13T08:00:00Z",
      },
    ]),
  accountsSignOut: () => ok(null),
  authRespond: (requestId: string, response: unknown) => {
    // Like serde: only variants of the contract are accepted (credentials, certificate, cancel).
    const kind = (response as { kind?: unknown } | null)?.kind;
    if (kind !== "credentials" && kind !== "certificate" && kind !== "cancel") {
      return errCode(
        "internal",
        `invalid args \`response\` for command \`auth_respond\`: unknown variant ${String(kind)}`,
      );
    }
    mockAuthResponses.push({ requestId, response });
    return ok(null);
  },
  // The real backend shows a native file picker and a confirmation; the mock pretends the user
  // picked a diff tool and confirmed.
  programChoose: (kind: ProgramKind, args: string[]) => {
    const key = PROGRAM_KEYS[kind];
    Object.assign(mockSettings, { [key]: { path: "/usr/bin/meld", args } });
    return ok({ ...mockSettings });
  },
  programClear: (kind: ProgramKind) => {
    Object.assign(mockSettings, { [PROGRAM_KEYS[kind]]: null });
    return ok({ ...mockSettings });
  },
  // The real backend shows the native folder dialog; the mock pretends the user chose a folder.
  checkoutRootChoose: () => {
    Object.assign(mockSettings, { "general.checkoutRoot": "/Users/dev/projects" });
    return ok({ ...mockSettings });
  },
  settingsSet: (patch: SettingsPatch_Deserialize) => {
    Object.assign(mockSettings, patch);
    return ok({ ...mockSettings });
  },
};

// Commands are looked up lazily; anything not mocked resolves to a typed AppError instead of throwing.
// The Proxy cast is necessary because `Commands` has ~50 members and the mock covers only what the UI shell uses.
const commandsProxy = new Proxy({} as Record<string, unknown>, {
  get(_t, prop: string) {
    const fn = implemented[prop as keyof Commands];
    return (...args: unknown[]) => {
      mockCalls.push({ name: prop, args });
      return fn
        ? (fn as (...a: unknown[]) => unknown)(...args)
        : err(`mock: command "${prop}" is not implemented`);
    };
  },
}) as unknown as Commands;

export const mockAdapter: IpcAdapter = {
  kind: "mock",
  commands: commandsProxy,
  async listen(name, handler) {
    const set = listeners.get(name) ?? new Set<Handler>();
    listeners.set(name, set);
    set.add(handler);
    return () => {
      set.delete(handler);
    };
  },
};
