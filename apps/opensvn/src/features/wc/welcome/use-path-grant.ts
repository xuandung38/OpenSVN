import { useEffect } from "react";
import type { DroppedPaths, PathGrantInfo, WorkingCopy } from "../../../ipc/bindings";
import { ipc, unwrap } from "../../../ipc/client";

/**
 * Opens the native folder dialog and returns a single-use grant (never a raw path), or `null` when
 * the user cancels. Commands that need a folder outside a registered working copy take the grant id.
 */
export async function pickFolder(title: string): Promise<PathGrantInfo | null> {
  return unwrap(ipc().commands.pathPick({ kind: "folder", title }));
}

/** Picks a folder and registers it as a working copy. Resolves to `null` when cancelled. */
export async function addExistingWorkingCopy(title: string): Promise<WorkingCopy | null> {
  const grant = await pickFolder(title);
  if (!grant) return null;
  return unwrap(ipc().commands.wcAddExisting(grant.id));
}

function isDroppedPaths(p: unknown): p is DroppedPaths {
  return (
    typeof p === "object" &&
    p !== null &&
    Array.isArray((p as { grants?: unknown }).grants) &&
    ((p as DroppedPaths).grants as unknown[]).every(
      (g) =>
        typeof g === "object" &&
        g !== null &&
        typeof (g as { id?: unknown }).id === "string" &&
        ((g as { kind?: unknown }).kind === "dir" || (g as { kind?: unknown }).kind === "file"),
    )
  );
}

/**
 * Subscribes to `fs://dropped` (folders dropped onto the window arrive as grants, never as raw
 * paths). Each dropped folder is offered to `onFolder`; files are reported through `onIgnored`.
 */
export function useDroppedFolders(
  onFolder: (grant: PathGrantInfo) => void,
  onIgnored: () => void,
): void {
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    ipc()
      .listen("fs://dropped", (payload) => {
        if (!isDroppedPaths(payload)) return;
        for (const g of payload.grants) {
          if (g.kind === "dir") onFolder(g);
          else onIgnored();
        }
      })
      .then((u) => {
        if (cancelled) u();
        else unlisten = u;
      })
      .catch((e: unknown) => console.error("fs://dropped subscription failed", e));
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [onFolder, onIgnored]);
}
