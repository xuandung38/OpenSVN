import { QueryClient } from "@tanstack/react-query";
import { expect, test, vi } from "vitest";
import { unwrap } from "./client";
import { createEventBus, MAX_OPS, type OpState } from "./events";
import { mockAdapter, mockEmit } from "./mock/adapter";
import { queryKeys } from "./query-keys";

test("mock adapter serves working copies and unmocked commands return a typed error", async () => {
  const wcs = await unwrap(mockAdapter.commands.wcList());
  expect(wcs.length).toBeGreaterThan(0);
  await expect(unwrap(mockAdapter.commands.diagnosticsExport("token"))).rejects.toMatchObject({
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

  mockEmit("wc://status-changed", { wcId: "wc-main", changed: [], removed: [] });
  expect(invalidate).toHaveBeenCalledWith({ queryKey: queryKeys.status("wc-main") });

  const menu = vi.fn();
  bus.onMenu(menu);
  mockEmit("menu://commit");
  expect(menu).toHaveBeenCalledWith("menu://commit");

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
