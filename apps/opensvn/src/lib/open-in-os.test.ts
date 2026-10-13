import { expect, test, vi } from "vitest";
import type { AppError } from "../ipc/bindings";
import { openWithDefaultApp } from "./open-in-os";

const refused: AppError = {
  code: "invalid-argument",
  message: "This kind of item is not opened automatically because it could run code.",
  detail: null,
  path: "setup.exe",
  retryable: false,
  actions: ["reveal-in-file-manager"],
};

type Result = { status: "ok"; data: null } | { status: "error"; error: AppError };

function fake(openResult: Result) {
  return {
    openExternal: vi.fn(async (): Promise<Result> => openResult),
    revealInOs: vi.fn(async (): Promise<Result> => ({ status: "ok", data: null })),
  };
}

test("a document opens with the default app and is not revealed", async () => {
  const c = fake({ status: "ok", data: null });
  expect(await openWithDefaultApp(c, "wc1", "README.md")).toEqual({ kind: "opened" });
  expect(c.openExternal).toHaveBeenCalledWith("wc1", "README.md", "default");
  expect(c.revealInOs).not.toHaveBeenCalled();
});

test("a refused item is revealed in the file manager instead", async () => {
  const c = fake({ status: "error", error: refused });
  expect(await openWithDefaultApp(c, "wc1", "setup.exe")).toEqual({
    kind: "revealed",
    reason: refused,
  });
  expect(c.revealInOs).toHaveBeenCalledWith("wc1", "setup.exe");
});

test("other errors are returned without revealing", async () => {
  const missing: AppError = { ...refused, code: "path-not-found", actions: null };
  const c = fake({ status: "error", error: missing });
  expect(await openWithDefaultApp(c, "wc1", "gone.txt")).toEqual({
    kind: "failed",
    error: missing,
  });
  expect(c.revealInOs).not.toHaveBeenCalled();
});
