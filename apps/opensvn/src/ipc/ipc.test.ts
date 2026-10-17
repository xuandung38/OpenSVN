import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { expect, test, vi } from "vitest";
import { unwrap } from "./client";
import {
  createEventBus,
  EVENT_NAMES,
  isValidEventName,
  MAX_OPS,
  MENU_EVENTS,
  type OpState,
} from "./events";
import { mockAdapter, mockEmit } from "./mock/adapter";
import { queryKeys } from "./query-keys";

test("mock adapter serves working copies and unmocked commands return a typed error", async () => {
  const wcs = await unwrap(mockAdapter.commands.wcList());
  expect(wcs.length).toBeGreaterThan(0);
  await expect(unwrap(mockAdapter.commands.upgradeFormat("wc-main", 32))).rejects.toMatchObject({
    name: "IpcError",
  });
});

test("event bus folds op://* events into the ops cache and invalidates on wc://*", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  await bus.start();
  const invalidate = vi.spyOn(qc, "invalidateQueries");

  const handle = { opId: "o1", kind: "update", wcId: "wc-main", title: "Update" };
  mockEmit("op://started", handle);
  mockEmit("op://progress", { opId: "o1", phase: "update", current: 5, total: 10 });
  let ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.o1?.state).toBe("running");
  expect(ops.o1?.progress?.current).toBe(5);

  mockEmit("op://failed", { opId: "o1", error: { code: "network", message: "offline" } });
  ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.o1?.state).toBe("failed");
  expect(ops.o1?.error).toBe("offline");

  const listKey = [...queryKeys.status("wc-main"), { includeIgnored: false }];
  qc.setQueryData(listKey, { snapshot: 3, files: [] });
  mockEmit("wc://status-changed", {
    wcId: "wc-main",
    changed: [],
    removed: [],
    base: 1,
    snapshot: 2,
  });
  expect(invalidate).not.toHaveBeenCalled();
  mockEmit("wc://status-changed", {
    wcId: "wc-main",
    changed: [],
    removed: [],
    base: 5,
    snapshot: 6,
  });
  expect(invalidate).toHaveBeenCalledWith({ queryKey: listKey, exact: true });

  const menu = vi.fn();
  bus.onMenu(menu);
  mockEmit("menu://wc/commit");
  expect(menu).toHaveBeenCalledWith("menu://wc/commit");

  bus.stop();
  mockEmit("wc://incoming", { wcId: "wc-main", count: 1, latestRev: 2 });
  expect(invalidate).not.toHaveBeenCalledWith({ queryKey: queryKeys.workingCopies });
});

test("event bus: start -> stop -> start delivers each event once (no leaked listeners)", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  const handler = vi.fn();
  bus.on("auth://prompt", handler);
  // React StrictMode order: mount, cleanup, mount, with the first start() still in flight.
  const first = bus.start();
  bus.stop();
  const second = bus.start();
  await Promise.all([first, second]);
  mockEmit("auth://prompt", {
    requestId: "r",
    kind: "credentials",
    realm: "x",
    url: "u",
    cert: null,
  });
  expect(handler).toHaveBeenCalledTimes(1);
  bus.stop();
  mockEmit("auth://prompt", {
    requestId: "r2",
    kind: "credentials",
    realm: "x",
    url: "u",
    cert: null,
  });
  expect(handler).toHaveBeenCalledTimes(1);
});

test("event bus: stop() while start() is in flight leaves zero subscriptions", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  const handler = vi.fn();
  bus.on("op://started", handler);
  const p = bus.start();
  bus.stop();
  await p;
  mockEmit("op://started", { opId: "z", kind: "update", wcId: null, title: "t" });
  expect(handler).not.toHaveBeenCalled();
});

test("ops cache keeps only the most recent entries", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  await bus.start();
  for (let i = 0; i < MAX_OPS + 10; i++) {
    mockEmit("op://started", { opId: `op-${i}`, kind: "update", wcId: null, title: `t${i}` });
  }
  const ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(Object.keys(ops)).toHaveLength(MAX_OPS);
  expect(ops["op-0"]).toBeUndefined();
  expect(ops[`op-${MAX_OPS + 9}`]).toBeDefined();
  bus.stop();
});

test("string / unknown rejections become AppError internal with a non-empty message", async () => {
  await expect(unwrap(Promise.reject("invalid args `wcId` for command"))).rejects.toMatchObject({
    appError: { code: "internal", message: "invalid args `wcId` for command" },
  });
  await expect(unwrap(Promise.reject(undefined))).rejects.toMatchObject({
    appError: { code: "internal", message: "Unexpected internal error" },
  });
  await expect(unwrap(Promise.reject(new Error("boom")))).rejects.toMatchObject({
    message: "boom",
  });
  // Malformed error objects inside an `error` result are normalised too.
  await expect(
    unwrap(Promise.resolve({ status: "error" as const, error: "nope" as never })),
  ).rejects.toMatchObject({ appError: { code: "internal", message: "nope" } });
  // Well-formed AppError passes through untouched.
  const app = {
    code: "network",
    message: "offline",
    detail: null,
    path: null,
    retryable: true,
    actions: null,
  };
  await expect(
    unwrap(Promise.resolve({ status: "error" as const, error: app as never })),
  ).rejects.toMatchObject({ appError: app });
});

test("read-only operations do not refetch repository views, writing ones do", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  await bus.start();
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  mockEmit("op://started", { opId: "b1", kind: "blame", wcId: null, title: "Blame" });
  mockEmit("op://finished", { opId: "b1", result: { kind: "done" } });
  expect(invalidate).not.toHaveBeenCalledWith({ queryKey: queryKeys.repoRoot });
  expect(invalidate).not.toHaveBeenCalledWith({ queryKey: queryKeys.logRoot });
  mockEmit("op://started", { opId: "e1", kind: "remote-edit", wcId: null, title: "Edit" });
  mockEmit("op://finished", { opId: "e1", result: { kind: "done" } });
  expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.repoRoot });
  expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.logRoot });
  bus.stop();
});

test("mock data never carries lock tokens, like the IPC contract", async () => {
  const { mockStatus } = await import("./mock/data");
  const locks = mockStatus.flatMap((f) => (f.lock ? [f.lock] : []));
  expect(locks.length).toBeGreaterThan(0);
  for (const lock of locks) expect(lock.token).toBeNull();
});

test("event bus: notice phases update banners, never the operation's progress", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  await bus.start();
  mockEmit("op://started", { opId: "n1", kind: "commit", wcId: "wc-main", title: "Commit" });
  mockEmit("op://progress", { opId: "n1", phase: "commit", current: 3, total: 3 });
  mockEmit("op://progress", {
    opId: "n1",
    phase: "needs-cleanup",
    currentPath: "file in use",
  });
  mockEmit("op://progress", { opId: "n1", phase: "nested-wc-skipped", currentPath: "x" });
  const ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.n1?.progress?.phase).toBe("commit");
  expect(qc.getQueryData(queryKeys.banners("wc-main"))).toEqual([
    { wcId: "wc-main", kind: "needs-cleanup", data: ["file in use"] },
  ]);
  bus.stop();
});

test("every subscribed event name is one Tauri accepts", () => {
  for (const name of [...EVENT_NAMES, ...MENU_EVENTS]) {
    expect(isValidEventName(name), name).toBe(true);
  }
  expect(MENU_EVENTS).toContain("menu://app/settings");
  // Tauri refuses `.` (and spaces, CR, ...) in event names, on emit and on listen.
  for (const bad of ["menu://app.settings", "", "a b", "x\r"]) {
    expect(isValidEventName(bad), bad).toBe(false);
  }
});

test("event bus: one refused subscription does not stop the later ones", async () => {
  const qc = new QueryClient();
  const refused = "menu://file/checkout";
  const adapter = {
    ...mockAdapter,
    listen: (name: string, handler: (payload: unknown) => void) =>
      name === refused ? Promise.reject(new Error("refused")) : mockAdapter.listen(name, handler),
  };
  const bus = createEventBus(adapter, qc);
  const menu = vi.fn();
  bus.onMenu(menu);
  await expect(bus.start()).rejects.toThrow(/menu:\/\/file\/checkout: refused/);
  mockEmit("menu://app/settings");
  expect(menu).toHaveBeenCalledWith("menu://app/settings");
  bus.stop();
});

test("event bus: path logs, warnings and results stay with the operation; background checks leave", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  await bus.start();
  mockEmit("op://started", {
    opId: "k1",
    kind: "checkout",
    wcId: null,
    title: "Checkout REPO",
    rootPath: "/w/REPO",
    background: false,
  });
  mockEmit("op://progress", {
    opId: "k1",
    phase: "checkout",
    current: 2,
    entries: [
      { action: "added", path: "" },
      { action: "added", path: "a" },
    ],
    entriesDropped: 0,
  });
  mockEmit("op://progress", { opId: "k1", phase: "external-warning", currentPath: "ext" });
  mockEmit("op://progress", {
    opId: "k1",
    phase: "checkout",
    current: 3,
    entries: [{ action: "added", path: "b" }],
    entriesDropped: 4,
  });
  mockEmit("op://finished", { opId: "k1", result: { kind: "checkedOut", wcId: "w9", newRev: 7 } });
  let ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.k1?.log.map((l) => l.path)).toEqual(["", "a", "b"]);
  expect(ops.k1?.logDropped).toBe(4);
  expect(ops.k1?.warnings).toEqual(["ext"]);
  expect(ops.k1?.progress?.current).toBe(3);
  expect(ops.k1?.result).toEqual({ kind: "checkedOut", wcId: "w9", newRev: 7 });
  expect(ops.k1?.endedAt).not.toBeNull();

  const bg = { opId: "b1", kind: "status-remote", wcId: "w9", title: "Check", rootPath: null };
  mockEmit("op://started", { ...bg, background: true });
  mockEmit("op://finished", { opId: "b1", result: { kind: "done" } });
  ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.b1).toBeUndefined();
  mockEmit("op://started", { ...bg, opId: "b2", background: true });
  mockEmit("op://failed", { opId: "b2", error: { code: "network", message: "offline" } });
  ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.b2?.state).toBe("failed");
  expect(ops.b2?.appError?.message).toBe("offline");
  // Every round fails while offline: only the newest failure of each working copy stays.
  mockEmit("op://started", { ...bg, opId: "b3", background: true });
  mockEmit("op://failed", { opId: "b3", error: { code: "network", message: "offline" } });
  mockEmit("op://started", { ...bg, opId: "b4", wcId: "w8", background: true });
  mockEmit("op://failed", { opId: "b4", error: { code: "network", message: "offline" } });
  ops = qc.getQueryData<Record<string, OpState>>(queryKeys.ops) ?? {};
  expect(ops.b2).toBeUndefined();
  expect(ops.b3?.state).toBe("failed");
  expect(ops.b4?.state).toBe("failed");
  expect(ops.k1?.state).toBe("done");
  bus.stop();
});

test("a revision change drops a log still loading and reads it again", async () => {
  const qc = new QueryClient();
  const bus = createEventBus(mockAdapter, qc);
  await bus.start();
  let calls = 0;
  const key = queryKeys.log("wc-main", "", 100);
  // A mounted view of the log (the History sidebar).
  const observer = new QueryObserver(qc, {
    queryKey: key,
    queryFn: () => {
      calls++;
      // The first read never ends (a page the server needs minutes for); the second is quick.
      return calls === 1 ? new Promise<string>(() => {}) : Promise.resolve("fresh");
    },
  });
  const unsubscribe = observer.subscribe(() => {});
  await vi.waitFor(() => expect(calls).toBe(1));
  mockEmit("wc://revision-changed", { wcId: "wc-main", baseRev: 2, revRange: null });
  await vi.waitFor(() => expect(qc.getQueryData(key)).toBe("fresh"));
  expect(calls).toBe(2);
  unsubscribe();
  bus.stop();
});
