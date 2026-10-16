import { MoreHorizontal } from "lucide-react";
import { useState } from "react";
import { Button } from "../../../components/ui/button";
import { DropdownMenu } from "../../../components/ui/dropdown-menu";
import { EmptyState } from "../../../components/ui/empty-state";
import { SafeText } from "../../../components/ui/safe-text";
import { StatusGlyph } from "../../../components/ui/status-glyph";
import { useToast } from "../../../components/ui/toast";
import type { ChangedPath, LogEntry, LogTarget } from "../../../ipc/bindings";
import { useSettings } from "../../../ipc/queries";
import { openExternalUrl } from "../../../lib/external-link";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { urlFromRepoPath } from "../repo-url";
import { historyStrings } from "./history-strings";
import { LinkifiedText } from "./linkified-text";
import { RevisionDiffDialog } from "./revision-diff-dialog";
import { useRevisionPaths } from "./use-history";
import { expandWebUrl } from "./web-url";

export interface RevisionNav {
  /** Open `url` at `rev` in the repository browser. */
  openInBrowse?: (url: string, rev: number) => void;
  blameAt?: (url: string, rev: number) => void;
  showLogOf?: (url: string) => void;
}

/** Revision header, message and changed paths (doc 01, 3.4 and 6.2/6.3). */
export function RevisionDetails({
  entry,
  target,
  merged = false,
  repoRootUrl,
  nav,
  compareRev = null,
  onMarkCompare,
}: {
  entry: LogEntry | null;
  /** The logged node; the changed paths of `entry` are read from it when the list has none. */
  target?: LogTarget;
  /** `entry` came in through a merge (not part of `target`'s own history). */
  merged?: boolean;
  repoRootUrl: string;
  nav: RevisionNav;
  /** Revision marked for a comparison of two revisions, if any. */
  compareRev?: number | null;
  /** Marks (or, for the marked one, unmarks) a revision for comparison. */
  onMarkCompare?: (rev: number) => void;
}) {
  const t = useT(historyStrings);
  const c = useT(commonStrings);
  const toast = useToast();
  const settings = useSettings();
  const [diffPath, setDiffPath] = useState<string | null>(null);
  const needPaths = entry !== null && entry.changedPaths == null && target !== undefined;
  const paths = useRevisionPaths(
    target ?? { kind: "url", url: repoRootUrl },
    needPaths ? { from: entry.rev, to: entry.rev } : null,
    repoRootUrl,
    merged,
  );

  if (!entry) {
    return <EmptyState title={t("selectRevision")} description={t("selectRevisionHint")} />;
  }

  const copy = (text: string) => {
    navigator.clipboard
      .writeText(text)
      .then(() => toast.show(t("copied")))
      .catch((e: unknown) => toast.show(String(e)));
  };
  const webTemplate = settings.data?.["integrations.webRevisionUrl"] ?? "";
  const webUrl = expandWebUrl(webTemplate, { repoRootUrl, rev: entry.rev });
  const changedPaths = needPaths
    ? (paths.data?.find((e) => e.rev === entry.rev)?.changedPaths ?? null)
    : entry.changedPaths;

  const pathMenu = (p: ChangedPath) => {
    const url = urlFromRepoPath(repoRootUrl, p.path);
    return [
      { label: t("showDiff"), onSelect: () => setDiffPath(p.path) },
      ...(p.kind !== "dir" && p.action !== "D"
        ? [
            { label: t("openAtRev"), onSelect: () => nav.openInBrowse?.(url, entry.rev) },
            { label: t("blameAtRev"), onSelect: () => nav.blameAt?.(url, entry.rev) },
          ]
        : []),
      { label: t("showPathLog"), onSelect: () => nav.showLogOf?.(url) },
    ];
  };

  return (
    <div className="flex h-full min-h-0 flex-col overflow-auto">
      <header className="flex items-start gap-2 border-b border-line px-3.5 py-2.5">
        <div className="min-w-0 flex-1">
          <div className="tnum flex flex-wrap items-baseline gap-x-2">
            <b className="font-mono">r{entry.rev}</b>
            <SafeText>{entry.author ?? c("unknownAuthor")}</SafeText>
            <time dateTime={entry.date} className="text-muted-text">
              {new Date(entry.date).toLocaleString()}
            </time>
          </div>
        </div>
        <DropdownMenu
          trigger={
            <Button small aria-label={t("actions")}>
              <MoreHorizontal size={14} />
            </Button>
          }
          entries={[
            { label: t("copyRev"), onSelect: () => copy(String(entry.rev)) },
            { label: t("copyMessage"), onSelect: () => copy(entry.message) },
            ...(onMarkCompare
              ? [
                  {
                    label:
                      compareRev === entry.rev
                        ? t("unmarkCompare", { rev: entry.rev })
                        : t("markCompare", { rev: entry.rev }),
                    onSelect: () => onMarkCompare(entry.rev),
                  },
                ]
              : []),
            "separator",
            {
              label: t("openWeb"),
              disabled: webUrl === null,
              onSelect: () => {
                if (webUrl) openExternalUrl(webUrl).catch((e: unknown) => toast.show(String(e)));
              },
            },
          ]}
        />
      </header>
      <section className="border-b border-line px-3.5 py-2.5" aria-label="Message">
        <LinkifiedText text={entry.message} />
      </section>
      <section className="px-3.5 py-2.5" aria-label={t("changedPaths")}>
        <h3 className="m-0 mb-1.5 text-[10px] font-medium tracking-[.06em] text-muted-text uppercase">
          {t("changedPaths")}
        </h3>
        {needPaths && paths.isPending ? (
          <p className="m-0 text-muted-text">{t("loadingPaths")}</p>
        ) : needPaths && paths.isError ? (
          <SafeText as="p" className="m-0 text-bad-text">
            {t("pathsFailed", { message: paths.error.message })}
          </SafeText>
        ) : (changedPaths ?? []).length === 0 ? (
          <p className="m-0 text-muted-text">{t("noPaths")}</p>
        ) : (
          <ul className="m-0 list-none p-0">
            {changedPaths?.map((p) => (
              <li key={`${p.action}:${p.path}`} className="flex items-center gap-1.5 py-0.5">
                <StatusGlyph code={p.action} />
                <span className="min-w-0 flex-1 truncate font-mono text-[12px]" title={p.path}>
                  <SafeText>{p.path}</SafeText>
                  {p.copyFrom && (
                    <span className="ml-2 text-muted-text">
                      ⧉{" "}
                      <SafeText>
                        {t("copyFrom", { path: p.copyFrom.path, rev: p.copyFrom.rev })}
                      </SafeText>
                    </span>
                  )}
                </span>
                <DropdownMenu
                  trigger={
                    <Button small aria-label={`${t("actions")}: ${p.path}`}>
                      <MoreHorizontal size={14} />
                    </Button>
                  }
                  entries={pathMenu(p)}
                />
              </li>
            ))}
          </ul>
        )}
      </section>
      {diffPath && (
        <RevisionDiffDialog
          url={urlFromRepoPath(repoRootUrl, diffPath)}
          rev={entry.rev}
          path={diffPath}
          onClose={() => setDiffPath(null)}
        />
      )}
    </div>
  );
}
