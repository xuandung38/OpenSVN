import type { QueryClient } from "@tanstack/react-query";
import type { IpcAdapter, Unlisten } from "./adapter";
import type {
  ActionCounts,
  AppError,
  AuthPromptEvent,
  Banner,
  Incoming,
  OperationHandle,
  OpFailed,
  OpFinished,
  OpProgress,
  OpResult,
  PathAction,
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

/**
 * Notice phases of `op://progress` (Rust `engine::context::PHASE_*`): not counts but news for the
 * user, never dropped by the backend's throttle. `currentPath` carries the text.
 */
export const PHASE_NEEDS_CLEANUP = "needs-cleanup";
export const PHASE_NESTED_WC_SKIPPED = "nested-wc-skipped";
/** An external failed or was skipped; `currentPath` is its path. Collected as an operation warning. */
export const PHASE_EXTERNAL_WARNING = "external-warning";

/**
 * Native menu events (`menu://wc/update`, `menu://file/checkout`, ...) carry no payload. The item
 * id's dots become slashes (`menu.rs` `event_name`): Tauri refuses any other character set.
 */
export type MenuEventName = `menu://${string}`;

/**
 * Tauri's rule for event names: only alphanumerics, `-`, `/`, `:` and `_`. Anything else fails both
 * `emit` in Rust and `listen` here, so the event never arrives.
 */
export function isValidEventName(name: string): boolean {
  return /^[\p{L}\p{N}\-/:_]+$/u.test(name);
}

/** One line of an operation's path log (`A  src/lib.rs`). */
export interface LogLine {
  action: PathAction;
  path: string;
}

/** Path log lines kept per operation (the details panel virtualises them). */
export const OP_LOG_LIMIT = 5000;

/** Live view of one operation for the Activity panel and its details panel. */
export interface OpState {
  handle: OperationHandle;
  state: "running" | "done" | "failed";
  progress: OpProgress | null;
  /** Backend message of the failure (kept for callers that only need the text). */
  error: string | null;
  /** The full failure, for the short localised text and its "Details". */
  appError: AppError | null;
  /** Result of a finished operation (final revision). */
  result: OpResult | null;
  /** Most recent paths, oldest first, at most `OP_LOG_LIMIT`. */
  log: LogLine[];
  /** Paths reported but no longer in `log` (older ones, or coalesced away by the backend). */
  logDropped: number;
  /** Notices worth keeping in the summary (externals, skipped nested working copies). */
  warnings: string[];
  /** Wall-clock times (ms) for the elapsed time and the transfer rate. */
  startedAt: number;
  endedAt: number | null;
  /** Bytes per second, smoothed; `null` until two byte samples arrived. */
  rate: number | null;
  rateMark: { bytes: number; at: number } | null;
}

/** Fresh state of an operation that just started. */
export function newOpState(handle: OperationHandle, now: number = Date.now()): OpState {
  return {
    handle,
    state: "running",
    progress: null,
    error: null,
    appError: null,
    result: null,
    log: [],
    logDropped: 0,
    warnings: [],
    startedAt: now,
    endedAt: null,
    rate: null,
    rateMark: null,
  };
}

/** Rate samples closer together than this are merged (avoids jumpy numbers at 20 events/s). */
const RATE_WINDOW_MS = 500;

/**
 * Applies one coalesced progress sample: appends its paths to the log (keeping the newest
 * `OP_LOG_LIMIT`) and updates the smoothed transfer rate. Pure, so it is unit-tested directly.
 */
export function applyProgress(cur: OpState, p: OpProgress, now: number = Date.now()): OpState {
  let { log, logDropped, rate, rateMark } = cur;
  logDropped += p.entriesDropped ?? 0;
  if (p.entries?.length) {
    const merged = log.concat(p.entries);
    const over = merged.length - OP_LOG_LIMIT;
    log = over > 0 ? merged.slice(over) : merged;
    if (over > 0) logDropped += over;
  }
  if (p.bytesDone != null) {
    if (!rateMark) {
      rateMark = { bytes: 0, at: cur.startedAt };
    }
    const dt = now - rateMark.at;
    if (dt >= RATE_WINDOW_MS) {
      const inst = (Math.max(0, p.bytesDone - rateMark.bytes) * 1000) / dt;
      rate = rate === null ? inst : rate * 0.6 + inst * 0.4;
      rateMark = { bytes: p.bytesDone, at: now };
    }
  }
  return { ...cur, progress: p, log, logDropped, rate, rateMark };
}

/** Counts of the finished (or running) operation, `null` when it reported no paths. */
export function opCounts(op: OpState): ActionCounts | null {
  return op.progress?.counts ?? null;
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

/**
 * Every native menu item the UI handles, as emitted by the one native menu (`src-tauri/src/menu.rs`,
 * `menu://<item id>`); listed so the bus can subscribe without wildcards. Rust tests check both
 * directions: each id is an item of the menu, and each item is listed here unless Rust handles it
 * itself (zoom, Exit). `MENU_ACTIONS` (`use-wc-shortcuts.ts`) has exactly these keys.
 */
export const MENU_EVENTS: readonly MenuEventName[] = [
  "menu://file/checkout",
  "menu://file/add-wc",
  "menu://file/switch-repo",
  "menu://file/recent",
  "menu://file/close-wc",
  "menu://app/settings",
  "menu://app/check-app-updates",
  "menu://edit/find",
  "menu://edit/copy-path",
  "menu://edit/copy-url",
  "menu://view/changes",
  "menu://view/history",
  "menu://view/browse",
  "menu://view/activity",
  "menu://view/sidebar",
  "menu://view/diff-mode",
  "menu://view/hide-whitespace",
  "menu://view/show-unversioned",
  "menu://view/show-ignored",
  "menu://view/group-changelist",
  "menu://view/tree",
  "menu://view/theme-system",
  "menu://view/theme-light",
  "menu://view/theme-dark",
  "menu://wc/check-updates",
  "menu://wc/update",
  "menu://wc/update-to",
  "menu://wc/commit",
  "menu://wc/discard-all",
  "menu://wc/sparse",
  "menu://wc/cleanup",
  "menu://wc/properties",
  "menu://wc/layout",
  "menu://wc/upgrade",
  "menu://wc/reveal",
  "menu://wc/terminal",
  "menu://wc/editor",
  "menu://branch/switch",
  "menu://branch/new",
  "menu://branch/new-tag",
  "menu://branch/merge",
  "menu://branch/compare",
  "menu://branch/show-on-server",
  "menu://branch/layout",
  "menu://sel/discard",
  "menu://sel/lock",
  "menu://sel/unlock",
  "menu://sel/changelist",
  "menu://sel/ignore",
  "menu://sel/resolve",
  "menu://sel/log",
  "menu://sel/blame",
  "menu://sel/rename",
  "menu://sel/delete",
  "menu://sel/properties",
  "menu://window/conflicts",
  "menu://help/docs",
  "menu://help/shortcuts",
  "menu://help/logs",
  "menu://help/report",
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
      // One refused subscription must not leave every later event (and menu item) dead: each one
      // is tried, and the failures are reported together once the others are active.
      const failed: string[] = [];
      const trySubscribe = async (name: string, cb: (payload: unknown) => void) => {
        try {
          return await subscribe(name, cb);
        } catch (e) {
          failed.push(`${name}: ${e instanceof Error ? e.message : String(e)}`);
          return true;
        }
      };
      starting = (async () => {
        for (const name of EVENT_NAMES) {
          if (!(await trySubscribe(name, (p) => dispatch(name, p)))) return;
        }
        for (const name of menuEvents) {
          const ok = await trySubscribe(name, () => {
            for (const h of menuHandlers) h(name);
          });
          if (!ok) return;
        }
        if (failed.length > 0) throw new Error(`could not subscribe to ${failed.join("; ")}`);
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
      updateOp(qc, handle.opId, () => newOpState(handle));
      break;
    }
    case "op://progress": {
      const p = payload as OpProgress;
      if (p.phase === PHASE_NEEDS_CLEANUP) {
        // The operation went through but left work for `cleanup` (e.g. a committed file in
        // use): the working copy shows its "needs cleanup" banner, as for a stale lock.
        const wcId = qc.getQueryData<Record<string, OpState>>(queryKeys.ops)?.[p.opId]?.handle.wcId;
        if (wcId) {
          const banner: Banner = {
            wcId,
            kind: "needs-cleanup",
            data: p.currentPath ? [p.currentPath] : [],
          };
          qc.setQueryData<Banner[]>(queryKeys.banners(wcId), (cur) => [
            ...(cur ?? []).filter((b) => b.kind !== banner.kind),
            banner,
          ]);
        }
        break;
      }
      if (p.phase === PHASE_NESTED_WC_SKIPPED || p.phase === PHASE_EXTERNAL_WARNING) {
        // News, not progress (the nested working copy is also a toast): kept for the summary.
        const text = p.currentPath ?? "";
        if (text) {
          updateOp(qc, p.opId, (cur) =>
            cur ? { ...cur, warnings: [...cur.warnings, text].slice(-100) } : cur,
          );
        }
        break;
      }
      updateOp(qc, p.opId, (cur) => (cur ? applyProgress(cur, p) : cur));
      break;
    }
    case "op://finished": {
      const p = payload as OpFinished;
      setOutcome(qc, p.opId, { status: "ok", result: p.result });
      // Refetch repository views only for operations that change the server. Read-only ones (blame,
      // log, status-remote) must not trigger refetching every loaded log page and open folder.
      if (changesRepository(p.opId, qc)) {
        void qc.invalidateQueries({ queryKey: queryKeys.repoRoot });
        void qc.invalidateQueries({ queryKey: queryKeys.logRoot });
      }
      const background = qc.getQueryData<Record<string, OpState>>(queryKeys.ops)?.[p.opId]?.handle
        .background;
      if (background) {
        // A background check that went through is not news: it leaves the Activity panel.
        removeOp(qc, p.opId);
        break;
      }
      updateOp(qc, p.opId, (cur) =>
        cur ? { ...cur, state: "done", result: p.result, endedAt: Date.now() } : cur,
      );
      break;
    }
    case "op://failed": {
      const p = payload as OpFailed;
      setOutcome(qc, p.opId, { status: "failed", error: p.error });
      const failed = qc.getQueryData<Record<string, OpState>>(queryKeys.ops)?.[p.opId]?.handle;
      if (failed?.background) {
        // Offline, a check fails every round: only the newest failure per working copy stays, so
        // they never push the user's own operations out of the panel (MAX_OPS).
        const ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
        for (const o of Object.values(ops)) {
          if (
            o.handle.opId !== p.opId &&
            o.handle.background &&
            o.state === "failed" &&
            o.handle.wcId === failed.wcId
          )
            removeOp(qc, o.handle.opId);
        }
      }
      updateOp(qc, p.opId, (cur) =>
        cur
          ? {
              ...cur,
              state: "failed",
              error: p.error.message,
              appError: p.error,
              endedAt: Date.now(),
            }
          : cur,
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

/** Drops an operation from the Activity panel (dismissed background failures, finished checks). */
export function removeOp(qc: QueryClient, opId: string) {
  qc.setQueryData<Record<string, OpState>>(queryKeys.ops, (cur = {}) => {
    if (!(opId in cur)) return cur;
    const { [opId]: _gone, ...rest } = cur;
    return rest;
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

/** Operation kinds that write to the repository (or its locks). */
const REPO_CHANGING_KINDS: ReadonlySet<OperationHandle["kind"]> = new Set([
  "commit",
  "lock",
  "unlock",
  "remote-edit",
  "switch",
  "merge",
]);

/** Unknown operations (finished before started was seen) are treated as changing: refetching is safe. */
export function changesRepository(opId: string, qc: QueryClient): boolean {
  const ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops);
  const kind = ops?.[opId]?.handle.kind;
  return kind === undefined || REPO_CHANGING_KINDS.has(kind);
}
