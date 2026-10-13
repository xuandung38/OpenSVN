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

test("a symlinked item: the default app refuses, the reveal fallback refuses too, and the user gets that refusal", async () => {
  const link: AppError = {
    ...refused,
    message: "This item is not opened automatically.",
    path: "notes.txt",
  };
  const revealRefused: AppError = {
    ...refused,
    message:
      "This item is a symbolic link. Only real files and folders inside the working copy are shown.",
    path: "notes.txt",
    actions: null,
  };
  const c = fake({ status: "error", error: link });
  c.revealInOs.mockResolvedValueOnce({ status: "error", error: revealRefused });
  const outcome = await openWithDefaultApp(c, "wc1", "notes.txt");
  expect(c.revealInOs).toHaveBeenCalledWith("wc1", "notes.txt");
  expect(outcome).toEqual({ kind: "failed", error: revealRefused });
  expect(outcome.kind === "failed" && outcome.error.message).toMatch(/symbolic link/);
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
