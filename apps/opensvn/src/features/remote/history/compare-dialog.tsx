import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { Dialog } from "../../../components/ui/dialog";
import { SafeText } from "../../../components/ui/safe-text";
import { StatusGlyph } from "../../../components/ui/status-glyph";
import type { ChangeAction, LogEntry } from "../../../ipc/bindings";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { urlFromRepoPath } from "../repo-url";
import { historyStrings } from "./history-strings";
import { RevisionDiffDialog } from "./revision-diff-dialog";

/**
 * Files changed after `from` up to `to`, as far as the loaded log knows, newest change first: the
 * candidates for a diff between the two revisions. Folders are left out (a diff is per file).
 */
export function filesBetween(
  entries: readonly LogEntry[],
  from: number,
  to: number,
): { path: string; action: ChangeAction }[] {
  const seen = new Map<string, ChangeAction>();
  for (const e of entries) {
    if (e.rev <= from || e.rev > to) continue;
    for (const p of e.changedPaths ?? []) {
      if (p.kind === "dir" || seen.has(p.path)) continue;
      seen.set(p.path, p.action);
    }
  }
  return [...seen].map(([path, action]) => ({ path, action }));
}

/** Compare two revisions: pick a changed file, then see `diff_rev(from, to)` of it. */
export function CompareDialog({
  entries,
  from,
  to,
  repoRootUrl,
  onClose,
}: {
  entries: readonly LogEntry[];
  from: number;
  to: number;
  repoRootUrl: string;
  onClose: () => void;
}) {
  const t = useT(historyStrings);
  const c = useT(commonStrings);
  const files = filesBetween(entries, from, to);
  const [open, setOpen] = useState<string | null>(
    files.length === 1 ? (files[0]?.path ?? null) : null,
  );

  if (open) {
    return (
      <RevisionDiffDialog
        url={urlFromRepoPath(repoRootUrl, open)}
        from={from}
        rev={to}
        path={open}
        onClose={files.length === 1 ? onClose : () => setOpen(null)}
      />
    );
  }
  return (
    <Dialog
      open
      onClose={onClose}
      title={t("compareFilesTitle", { from, to })}
      footer={<Button onClick={onClose}>{c("close")}</Button>}
    >
      {files.length === 0 ? (
        <p className="m-0">{t("compareNoFiles")}</p>
      ) : (
        <div className="flex flex-col gap-2">
          <p className="m-0 text-muted-text">{t("compareFilesHint", { from })}</p>
          <ul className="m-0 max-h-[50vh] list-none overflow-auto p-0">
            {files.map((f) => (
              <li key={f.path} className="flex items-center gap-1.5 py-0.5">
                <StatusGlyph code={f.action} />
                <Button
                  small
                  className="min-w-0 flex-1 justify-start truncate font-mono text-[12px]"
                  onClick={() => setOpen(f.path)}
                >
                  <SafeText>{f.path}</SafeText>
                </Button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </Dialog>
  );
}
