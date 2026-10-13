import { File, Folder } from "lucide-react";
import { VirtualList } from "../../../components/list/virtual-list";
import { ContextMenu } from "../../../components/ui/context-menu";
import { LockBadge } from "../../../components/ui/lock-badge";
import { SafeText } from "../../../components/ui/safe-text";
import type { RepoEntry } from "../../../ipc/bindings";
import { cn } from "../../../lib/cn";
import { formatBytes, relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { joinUrl } from "../repo-url";
import { browseStrings } from "./browse-strings";
import { type BrowseNode, type NodeActionHandlers, nodeMenuEntries } from "./node-actions";

const COLS = "grid-cols-[1fr_84px_64px_96px_92px_70px]";

/** Directory contents as a keyboard-navigable list (Enter opens, context menu / Actions for the rest). */
export function DirTable({
  dirUrl,
  entries,
  selectedUrl,
  handlers,
  onSelect,
}: {
  dirUrl: string;
  entries: RepoEntry[];
  selectedUrl: string | null;
  handlers: NodeActionHandlers;
  onSelect: (n: BrowseNode) => void;
}) {
  const t = useT(browseStrings);
  const c = useT(commonStrings);
  // Folders first, then files, each alphabetical: stable, scannable order.
  const sorted = [...entries].sort(
    (a, b) => (a.kind === b.kind ? 0 : a.kind === "dir" ? -1 : 1) || a.name.localeCompare(b.name),
  );
  const node = (e: RepoEntry): BrowseNode => ({ url: joinUrl(dirUrl, e.name), entry: e });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div
        aria-hidden="true"
        className={cn(
          "grid gap-2 border-b border-line bg-surface-2 px-3 py-1.5 text-[10px] font-medium tracking-[.06em] text-muted-text uppercase",
          COLS,
        )}
      >
        <span>{t("colName")}</span>
        <span>{t("colSize")}</span>
        <span>{t("colRev")}</span>
        <span>{t("colAuthor")}</span>
        <span>{t("colDate")}</span>
        <span>{t("colLock")}</span>
      </div>
      <VirtualList
        label={t("title")}
        className="min-h-0 flex-1"
        count={sorted.length}
        rowHeight={28}
        onActivate={(i) => {
          const e = sorted[i];
          if (e) handlers.open(node(e));
        }}
        renderRow={(i, ctx) => {
          const e = sorted[i];
          if (!e) return null;
          const n = node(e);
          return (
            <ContextMenu entries={nodeMenuEntries(n, handlers, t)}>
              {/* biome-ignore lint/a11y/useFocusableInteractive: focus stays on the listbox (aria-activedescendant) */}
              {/* biome-ignore lint/a11y/useKeyWithClickEvents: keyboard handled by the listbox */}
              <div
                id={ctx.id}
                role="option"
                aria-selected={n.url === selectedUrl}
                onClick={() => {
                  ctx.activate();
                  onSelect(n);
                }}
                onDoubleClick={() => handlers.open(n)}
                className={cn(
                  "grid h-full cursor-default items-center gap-2 px-3",
                  COLS,
                  n.url === selectedUrl ? "bg-sel" : "hover:bg-surface-2",
                  ctx.active &&
                    "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
                )}
              >
                <span className="flex min-w-0 items-center gap-1.5">
                  {e.kind === "dir" ? (
                    <Folder
                      size={14}
                      aria-label={t("dirKind")}
                      className="shrink-0 text-muted-text"
                    />
                  ) : (
                    <File
                      size={14}
                      aria-label={t("fileKind")}
                      className="shrink-0 text-muted-text"
                    />
                  )}
                  <SafeText className="truncate">{e.name}</SafeText>
                </span>
                <span className="tnum text-muted-text">
                  {e.size == null ? "" : formatBytes(e.size)}
                </span>
                <span className="tnum font-mono text-[12px]">r{e.createdRev}</span>
                <SafeText className="truncate">{e.lastAuthor ?? c("unknownAuthor")}</SafeText>
                <span className="tnum text-muted-text">{relativeTime(e.date)}</span>
                <span>{e.lock && <LockBadge owner={e.lock.mine ? "me" : "other"} />}</span>
              </div>
            </ContextMenu>
          );
        }}
      />
      {sorted.length === 0 && <p className="m-0 px-3 py-4 text-muted-text">{t("emptyFolder")}</p>}
    </div>
  );
}
