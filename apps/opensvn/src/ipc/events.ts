import type { QueryClient } from "@tanstack/react-query";
import type { IpcAdapter, Unlisten } from "./adapter";
import type {
  AppError,
  AuthPromptEvent,
  Banner,
  Incoming,
  OperationHandle,
  OpFailed,
  OpFinished,
  OpProgress,
  OpResult,
  RevisionChanged,
  StatusChanged,
} from "./bindings";
import { queryKeys } from "./query-keys";

/** Typed payloads for the backend events the UI consumes (names match `bindings.ts` events). */
export interface EventPayloads {
  "op://started": OperationHandle;
  "op://progress": OpProgress;
  "op://finished": OpFinished;
  "op://failed": OpFailed;
  "wc://status-changed": StatusChanged;
  "wc://revision-changed": RevisionChanged;
  "wc://incoming": Incoming;
  "wc://banner": Banner;
  "auth://prompt": AuthPromptEvent;
}
export type EventName = keyof EventPayloads;

export const EVENT_NAMES: EventName[] = [
  "op://started",
  "op://progress",
  "op://finished",
  "op://failed",
  "wc://status-changed",
  "wc://revision-changed",
  "wc://incoming",
  "wc://banner",
  "auth://prompt",
];

/** Native menu events (`menu://commit`, `menu://update`, ...) are free-form and carry no payload. */
export type MenuEventName = `menu://${string}`;

/** Live view of one operation for the Activity panel. */
export interface OpState {
  handle: OperationHandle;
  state: "running" | "done" | "failed";
  progress: OpProgress | null;
  error: string | null;
}

/** Final result of an operation, kept even when `op://finished` races ahead of `op://started`. */
export type OpOutcome = { status: "ok"; result: OpResult } | { status: "failed"; error: AppError };

/** Operations kept in the cache for the Activity panel. */
export const MAX_OPS = 50;

type Handler<T> = (payload: T) => void;

export interface EventBus {
  on<K extends EventName>(name: K, handler: Handler<EventPayloads[K]>): () => void;
  onMenu(handler: (name: MenuEventName) => void): () => void;
  /** Start listening to every backend event; resolves once subscriptions are active. */
  start(menuEvents?: readonly MenuEventName[]): Promise<void>;
  stop(): void;
}

/** Menu entries the native menu (P08) may emit; listed so the bus can subscribe without wildcards. */
export const MENU_EVENTS: readonly MenuEventName[] = [
  "menu://commit",
  "menu://update",
  "menu://refresh",
  "menu://add-working-copy",
  "menu://settings",
  "menu://about",
];

export function createEventBus(adapter: IpcAdapter, qc: QueryClient): EventBus {
  const handlers = new Map<string, Set<Handler<never>>>();
  const menuHandlers = new Set<(name: MenuEventName) => void>();
  const unlistens: Unlisten[] = [];
  // Bumped by stop(); a start() that finds a newer generation unregisters what it just subscribed.
  let generation = 0;
  let starting: Promise<void> | null = null;

  const dispatch = (name: EventName, payload: unknown) => {
    applyToCache(qc, name, payload);
    for (const h of handlers.get(name) ?? []) (h as Handler<unknown>)(payload);
  };

  return {
    on(name, handler) {
      const set = handlers.get(name) ?? new Set<Handler<never>>();
      handlers.set(name, set);
      set.add(handler as Handler<never>);
      return () => {
        set.delete(handler as Handler<never>);
      };
    },
    onMenu(handler) {
      menuHandlers.add(handler);
      return () => {
        menuHandlers.delete(handler);
      };
    },
    // Idempotent: concurrent/repeated calls share one subscription pass.
    start(menuEvents = MENU_EVENTS) {
      if (starting) return starting;
      const mine = ++generation;
      const subscribe = async (name: string, cb: (payload: unknown) => void) => {
        const u = await adapter.listen(name, cb);
        if (mine !== generation) {
          // stop() ran while this listen() was in flight: undo immediately instead of leaking.
          u();
          return false;
        }
        unlistens.push(u);
        return true;
      };
      starting = (async () => {
        for (const name of EVENT_NAMES) {
          if (!(await subscribe(name, (p) => dispatch(name, p)))) return;
        }
        for (const name of menuEvents) {
          const ok = await subscribe(name, () => {
            for (const h of menuHandlers) h(name);
          });
          if (!ok) return;
        }
      })();
      return starting;
    },
    stop() {
      generation++;
      starting = null;
      for (const u of unlistens.splice(0)) u();
    },
  };
}

/** Event -> TanStack Query cache. Payloads originate from our own backend and are typed per name. */
function applyToCache(qc: QueryClient, name: EventName, payload: unknown): void {
  switch (name) {
    case "wc://status-changed": {
      const p = payload as StatusChanged;
      void qc.invalidateQueries({ queryKey: queryKeys.status(p.wcId) });
      break;
    }
    case "wc://revision-changed": {
      const p = payload as RevisionChanged;
      void qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
      void qc.invalidateQueries({ queryKey: queryKeys.status(p.wcId) });
      void qc.invalidateQueries({ queryKey: queryKeys.logRoot });
      break;
    }
    case "wc://incoming":
      void qc.invalidateQueries({ queryKey: queryKeys.workingCopies });
      break;
    case "wc://banner": {
      const p = payload as Banner;
      qc.setQueryData<Banner[]>(queryKeys.banners(p.wcId), (cur) => [
        ...(cur ?? []).filter((b) => b.kind !== p.kind),
        p,
      ]);
      break;
    }
    case "op://started": {
      const handle = payload as OperationHandle;
      updateOp(qc, handle.opId, () => ({ handle, state: "running", progress: null, error: null }));
      break;
    }
    case "op://progress": {
      const p = payload as OpProgress;
      updateOp(qc, p.opId, (cur) => (cur ? { ...cur, progress: p } : cur));
      break;
    }
    case "op://finished": {
      const p = payload as OpFinished;
      setOutcome(qc, p.opId, { status: "ok", result: p.result });
      // A finished operation may have changed the server (remote edit, copy, lock): refetch active views.
      void qc.invalidateQueries({ queryKey: queryKeys.repoRoot });
      void qc.invalidateQueries({ queryKey: queryKeys.logRoot });
      updateOp(qc, p.opId, (cur) => (cur ? { ...cur, state: "done" } : cur));
      break;
    }
    case "op://failed": {
      const p = payload as OpFailed;
      setOutcome(qc, p.opId, { status: "failed", error: p.error });
      updateOp(qc, p.opId, (cur) =>
        cur ? { ...cur, state: "failed", error: p.error.message } : cur,
      );
      break;
    }
    case "auth://prompt":
      // Handled by subscribers (auth dialog), not cached.
      break;
  }
}

function updateOp(
  qc: QueryClient,
  opId: string,
  fn: (cur: OpState | undefined) => OpState | undefined,
) {
  qc.setQueryData<Record<string, OpState>>(queryKeys.ops, (cur = {}) => {
    const next = fn(cur[opId]);
    if (!next) return cur;
    const merged = { ...cur, [opId]: next };
    const ids = Object.keys(merged);
    if (ids.length <= MAX_OPS) return merged;
    // Keep only the most recent operations (insertion order): the Activity panel is not a history log.
    return Object.fromEntries(ids.slice(-MAX_OPS).map((id) => [id, merged[id] as OpState]));
  });
}

function setOutcome(qc: QueryClient, opId: string, outcome: OpOutcome) {
  qc.setQueryData<Record<string, OpOutcome>>(queryKeys.opOutcomes, (cur = {}) => {
    const merged = { ...cur, [opId]: outcome };
    const ids = Object.keys(merged);
    if (ids.length <= MAX_OPS) return merged;
    return Object.fromEntries(ids.slice(-MAX_OPS).map((id) => [id, merged[id] as OpOutcome]));
  });
}
