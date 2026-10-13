import type { Commands } from "../ipc/adapter";
import type { AppError, WcId } from "../ipc/bindings";

/** What happened when the user asked to open an item with the default app. */
export type OpenOutcome =
  | { kind: "opened" }
  /** The backend refused the default app (program, script, link, unknown type); the item was shown in the file manager instead. */
  | { kind: "revealed"; reason: AppError }
  | { kind: "failed"; error: AppError };

/**
 * Opens `path` of a working copy with the default app. When the backend refuses (its error carries
 * the `reveal-in-file-manager` action), the item is revealed in the file manager instead; the
 * caller shows `reason.message` so the user knows why.
 */
export async function openWithDefaultApp(
  commands: Pick<Commands, "openExternal" | "revealInOs">,
  wcId: WcId,
  path: string,
): Promise<OpenOutcome> {
  const opened = await commands.openExternal(wcId, path, "default");
  if (opened.status === "ok") return { kind: "opened" };
  if (!opened.error.actions?.includes("reveal-in-file-manager")) {
    return { kind: "failed", error: opened.error };
  }
  const revealed = await commands.revealInOs(wcId, path);
  if (revealed.status === "ok") return { kind: "revealed", reason: opened.error };
  return { kind: "failed", error: revealed.error };
}
