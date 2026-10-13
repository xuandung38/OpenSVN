import type { QueryClient } from "@tanstack/react-query";
import type { IpcAdapter, Unlisten } from "./adapter";
import type {
  AuthPromptEvent,
  Banner,
  Incoming,
  OperationHandle,
  OpFailed,
  OpFinished,
  OpProgress,
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
      return () => menuHandlers.delete(handler);
    },
    async start(menuEvents = MENU_EVENTS) {
      for (const name of EVENT_NAMES) {
        unlistens.push(await adapter.listen(name, (p) => dispatch(name, p)));
      }
      for (const name of menuEvents) {
        unlistens.push(
          await adapter.listen(name, () => {
            for (const h of menuHandlers) h(name);
          }),
        );
      }
    },
    stop() {
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
      updateOp(qc, p.opId, (cur) => (cur ? { ...cur, state: "done" } : cur));
      break;
    }
    case "op://failed": {
      const p = payload as OpFailed;
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
    return next ? { ...cur, [opId]: next } : cur;
  });
}
