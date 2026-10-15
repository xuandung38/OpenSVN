import { expect, test } from "vitest";
import type { AppError } from "../../ipc/bindings";
import { IpcError } from "../../ipc/client";
import { parentRepos } from "./parent-path";

const base: AppError = {
  code: "path-not-found",
  message: "list",
  detail: null,
  path: null,
  retryable: false,
  actions: null,
};
const repos = [{ name: "Dự án", url: "https://h/svn/D%E1%BB%B1%20%C3%A1n" }];

test("the repositories of a server URL come from its path-not-found error", () => {
  expect(parentRepos(new IpcError({ ...base, repositories: repos }))).toEqual(repos);
});

test("other errors, and a server listing nothing, show as themselves", () => {
  expect(parentRepos(new IpcError(base))).toBeNull();
  expect(parentRepos(new IpcError({ ...base, repositories: [] }))).toBeNull();
  expect(parentRepos(new IpcError({ ...base, repositories: null }))).toBeNull();
  for (const code of ["auth-required", "tls-untrusted", "cancelled", "network"] as const) {
    expect(parentRepos(new IpcError({ ...base, code, repositories: repos }))).toBeNull();
  }
  expect(parentRepos("boom")).toBeNull();
});
