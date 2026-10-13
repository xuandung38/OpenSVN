import type { IpcAdapter } from "./adapter";
import type { AppError } from "./bindings";
import { tauriAdapter } from "./tauri-adapter";

/** True when running inside the Tauri WebView (the IPC bridge injects this global). */
export function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** Mock data is allowed only for `pnpm dev:mock` / `pnpm dev` in a plain browser, never in release builds. */
export const MOCK_ALLOWED: boolean = import.meta.env.DEV || import.meta.env.MODE === "mock";

let current: IpcAdapter | null = null;

/**
 * Selects the transport once at startup. Inside Tauri the real bindings are always used. Without the
 * Tauri bridge, only dev/mock builds get the mock adapter (a dynamic import the bundler drops from
 * release builds, because `MOCK_ALLOWED` folds to `false`). A release build with no bridge is a hard
 * error: the UI must show a fatal screen, never fake data.
 */
export async function initIpc(): Promise<IpcAdapter> {
  if (current) return current;
  if (isTauriRuntime()) {
    current = tauriAdapter;
  } else if (MOCK_ALLOWED) {
    current = (await import("./mock/adapter")).mockAdapter;
  } else {
    throw new Error("Tauri IPC bridge not found (window.__TAURI_INTERNALS__ is missing).");
  }
  return current;
}

/** The active adapter. Throws if `initIpc()` has not completed (programming error). */
export function ipc(): IpcAdapter {
  if (!current) throw new Error("ipc() called before initIpc() resolved");
  return current;
}

/** Thrown by `unwrap` so TanStack Query surfaces the backend `AppError` unchanged. */
export class IpcError extends Error {
  readonly appError: AppError;
  constructor(appError: AppError) {
    super(appError.message);
    this.name = "IpcError";
    this.appError = appError;
  }
}

function isAppError(e: unknown): e is AppError {
  return (
    typeof e === "object" &&
    e !== null &&
    typeof (e as { code?: unknown }).code === "string" &&
    typeof (e as { message?: unknown }).message === "string"
  );
}

/**
 * Normalises anything a command can reject/throw with into an `AppError`. Tauri framework-level
 * failures (bad args, unknown command, panic) arrive as bare strings; they become `internal` with a
 * non-empty message so the UI never shows an empty toast or reads `code === undefined`.
 */
export function toAppError(e: unknown): AppError {
  if (e instanceof IpcError) return e.appError;
  if (isAppError(e)) return e;
  const message =
    typeof e === "string" && e.length > 0
      ? e
      : e instanceof Error && e.message.length > 0
        ? e.message
        : "Unexpected internal error";
  return { code: "internal", message, detail: null, path: null, retryable: false, actions: null };
}

type Result<T> = { status: "ok"; data: T } | { status: "error"; error: AppError };

/** Unwrap a tauri-specta `Result`: data on success, throws `IpcError` (always with a valid AppError). */
export async function unwrap<T>(call: Promise<Result<T>>): Promise<T> {
  let res: Result<T>;
  try {
    res = await call;
  } catch (e) {
    throw new IpcError(toAppError(e));
  }
  if (res.status === "error") throw new IpcError(toAppError(res.error));
  return res.data;
}
