import { Button } from "../../../components/ui/button";
import { ContextMenu } from "../../../components/ui/context-menu";
import { LockBadge } from "../../../components/ui/lock-badge";
import { SafeText } from "../../../components/ui/safe-text";
import type { FileStatus } from "../../../ipc/bindings";
import { useT } from "../../../lib/i18n/i18n";
import { useWcActions } from "../use-wc-actions";
import { displayPath } from "../wc-model";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";
import { fileMenuEntries } from "./file-menu";

/** Rows rendered at most; the Changes filter narrows the rest. */
export const LOCKS_SHOWN = 200;

/**
 * "Locks" section of the Changes tab (doc 01 §10.4, issue #128): unmodified files that need a lock
 * before editing (read-only `svn:needs-lock`) or that are locked, with Lock to edit / Unlock. They
 * are not changes: no commit tick, and the commit never includes them.
 */
export function LocksList({ files }: { files: readonly FileStatus[] }) {
  const session = useWcSession();
  const actions = useWcActions();
  const t = useT(wcStrings);
  if (files.length === 0) return null;
  const shown = files.slice(0, LOCKS_SHOWN);
  const rootLabel = t("wcRootRow", { name: session.wc?.name ?? "" });
  const open = (mode: "lock" | "unlock", f: FileStatus) =>
    session.openDialog({ kind: "lock", mode, paths: [f.path] });

  return (
    <section
      aria-label={t("locksSection", { count: files.length })}
      className="flex max-h-[35%] min-h-0 flex-col border-t border-line"
    >
      <h3
        title={t("locksSectionHint")}
        className="m-0 bg-surface-2 px-2.5 py-1 text-[11px] font-semibold text-muted-text"
      >
        {t("locksSection", { count: files.length })}
      </h3>
      <ul className="m-0 min-h-0 list-none overflow-auto p-0">
        {shown.map((f) => {
          const shownPath = displayPath(f.path, rootLabel);
          return (
            <ContextMenu key={f.path} entries={fileMenuEntries([f], session, actions, t)}>
              <li
                aria-label={shownPath}
                className="flex items-center gap-1.5 px-2.5 py-[3px] hover:bg-surface-2"
              >
                <span className="min-w-0 flex-1 truncate font-mono text-[12px]" title={shownPath}>
                  <SafeText>{shownPath}</SafeText>
                </span>
                {f.lock ? (
                  <LockBadge owner={f.lock.mine ? "me" : "other"} />
                ) : (
                  <span className="rounded-[4px] border border-line px-1 text-[11px] text-muted-text">
                    {t("needsLockBadge")}
                  </span>
                )}
                {!f.lock?.mine && (
                  <Button small onClick={() => open("lock", f)}>
                    {t("lockToEdit")}
                  </Button>
                )}
                {f.lock && (
                  <Button small onClick={() => open("unlock", f)}>
                    {t("unlockFile")}
                  </Button>
                )}
              </li>
            </ContextMenu>
          );
        })}
        {files.length > shown.length && (
          <li className="px-2.5 py-1 text-[11px] text-muted-text">
            +{files.length - shown.length}
          </li>
        )}
      </ul>
    </section>
  );
}
