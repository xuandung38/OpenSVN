import type { AppError } from "../../ipc/bindings";
import type { UpdateKey } from "./update-strings";

/** How long the daily check waits after the last one (ms). */
export const AUTO_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;
const LAST_AUTO_CHECK_KEY = "opensvn.updates.lastAutoCheck";

function storage(): Storage | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

/** When the last automatic check ran (ms since epoch), or `null` if never (or unreadable). */
export function readLastAutoCheck(): number | null {
  const v = Number(storage()?.getItem(LAST_AUTO_CHECK_KEY) ?? Number.NaN);
  return Number.isFinite(v) ? v : null;
}

export function writeLastAutoCheck(at: number): void {
  try {
    storage()?.setItem(LAST_AUTO_CHECK_KEY, String(at));
  } catch {
    // Private mode or full storage: the next start checks again, which is harmless.
  }
}

/**
 * Whether the start-up check is due: at most once per `AUTO_CHECK_INTERVAL_MS`. A last check in
 * the future (clock moved back) counts as due, so a wrong clock cannot stop checks for good.
 */
export function autoCheckDue(now: number, last: number | null): boolean {
  return last === null || last > now || now - last >= AUTO_CHECK_INTERVAL_MS;
}

/** Download progress in percent (0–100), or `null` when the size is unknown. */
export function percentOf(downloaded: number, total: number | null): number | null {
  if (!total || total <= 0) return null;
  return Math.min(100, Math.round((downloaded / total) * 100));
}

/**
 * Short localised text for a failed check or install: a refused token and an unreachable server
 * have their own sentence; anything else keeps the backend's message.
 */
export function updateErrorKey(e: Pick<AppError, "code">): UpdateKey | null {
  if (e.code === "forbidden") return "errForbidden";
  if (e.code === "network" || e.code === "timeout") return "errNetwork";
  return null;
}
