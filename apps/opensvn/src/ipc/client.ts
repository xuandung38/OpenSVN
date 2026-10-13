import type { IpcAdapter } from "./adapter";
import type { AppError } from "./bindings";
import { mockAdapter } from "./mock/adapter";
import { tauriAdapter } from "./tauri-adapter";

/** True when running inside the Tauri WebView (the IPC bridge injects this global). */
export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * `pnpm dev:mock` (vite --mode mock) or any plain browser without the Tauri bridge uses the mock
 * adapter; inside Tauri the real bindings are used.
 */
export function selectAdapter(): IpcAdapter {
  if (import.meta.env.MODE === "mock") return mockAdapter;
  return isTauriRuntime() ? tauriAdapter : mockAdapter;
}

export const ipc: IpcAdapter = selectAdapter();

/** Thrown by `unwrap` so TanStack Query surfaces the backend `AppError` unchanged. */
export class IpcError extends Error {
  readonly appError: AppError;
  constructor(appError: AppError) {
    super(appError.message);
    this.name = "IpcError";
    this.appError = appError;
  }
}

type Result<T> = { status: "ok"; data: T } | { status: "error"; error: AppError };

/** Unwrap a tauri-specta `Result`: data on success, throws `IpcError` on failure. */
export async function unwrap<T>(call: Promise<Result<T>>): Promise<T> {
  const res = await call;
  if (res.status === "error") throw new IpcError(res.error);
  return res.data;
}
