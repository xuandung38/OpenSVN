import { QueryClient } from "@tanstack/react-query";
import { expect, test, vi } from "vitest";
import { unwrap } from "./client";
import { createEventBus, type OpState } from "./events";
import { mockAdapter, mockEmit } from "./mock/adapter";
import { queryKeys } from "./query-keys";

test("mock adapter serves working copies and unmocked commands return a typed error", async () => {
  const wcs = await unwrap(mockAdapter.commands.wcList());
  expect(wcs.length).toBeGreaterThan(0);
  await expect(unwrap(mockAdapter.commands.diagnosticsExport())).rejects.toMatchObject({
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
