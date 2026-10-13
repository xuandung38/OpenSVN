import type { Commands, IpcAdapter } from "../adapter";
import type { AppError, OperationHandle, OperationKind, UpdateRequest } from "../bindings";
import { MOCK_WC_ID, mockLog, mockSettings, mockStatus, mockWorkingCopies } from "./data";

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

let opSeq = 0;
/** Simulates a long-running operation: op://started, a few op://progress ticks, op://finished. */
function simulateOp(kind: OperationKind, title: string, wcId: string | null): OperationHandle {
  const handle: OperationHandle = { opId: `mock-op-${++opSeq}`, kind, wcId, title };
  setTimeout(() => mockEmit("op://started", handle), 0);
  for (const pct of [25, 50, 75]) {
    setTimeout(
      () =>
        mockEmit("op://progress", {
          opId: handle.opId,
          phase: kind,
          current: pct,
          total: 100,
          bytesDone: null,
          bytesTotal: null,
          currentPath: null,
        }),
      pct * 20,
    );
  }
  setTimeout(() => {
    mockEmit("op://finished", { opId: handle.opId, result: { kind: "done" } });
    if (wcId) mockEmit("wc://status-changed", { wcId, changed: [], removed: [] });
  }, 2000);
  return handle;
}

const implemented: Partial<Record<keyof Commands, (...args: never[]) => unknown>> = {
  wcList: () => ok(mockWorkingCopies),
  statusGet: () => ok(mockStatus),
  logGet: () => ok({ entries: mockLog, nextRev: null }),
  settingsGet: () => ok(mockSettings),
  opList: () => ok([]),
  accountsList: () => ok([]),
  update: (req: UpdateRequest) =>
    ok(simulateOp("update", "Update to HEAD", req.wcId ?? MOCK_WC_ID)),
  commit: () => ok(simulateOp("commit", "Commit", MOCK_WC_ID)),
};

// Commands are looked up lazily; anything not mocked resolves to a typed AppError instead of throwing.
// The Proxy cast is necessary because `Commands` has ~50 members and the mock covers only what the UI shell uses.
const commandsProxy = new Proxy({} as Record<string, unknown>, {
  get(_t, prop: string) {
    const fn = implemented[prop as keyof Commands];
    return fn ?? (() => err(`mock: command "${prop}" is not implemented`));
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
