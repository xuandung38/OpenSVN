import { useEffect, useState } from "react";
import { RevisionRow } from "../../../components/list/revision-row";
import { VirtualList } from "../../../components/list/virtual-list";
import { Button } from "../../../components/ui/button";
import { EmptyState } from "../../../components/ui/empty-state";
import type { LogTarget } from "../../../ipc/bindings";
import { relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { HistoryFilterBar } from "./history-filters";
import { historyStrings } from "./history-strings";
import { RevisionDetails, type RevisionNav } from "./revision-details";
import { NO_FILTERS, useHistory } from "./use-history";

const ROW_H = 44;
/** Prefetch the next page when the keyboard gets this close to the end of the loaded list. */
const PREFETCH_MARGIN = 8;

/**
 * History tab: filters + paged revision list on the left, revision details on the right.
 * `target` is the scope (whole working copy, or a path/URL); `scopeLabel` names it in the chip.
 */
export function HistoryScreen({
  target,
  repoRootUrl,
  scopeLabel,
  onClearScope,
  focusRev,
  nav,
}: {
  target: LogTarget;
  repoRootUrl: string;
  scopeLabel: string | null;
  onClearScope?: () => void;
  /** Revision to select once loaded (e.g. from a blame line). */
  focusRev?: number | null;
  nav: RevisionNav;
}) {
  const t = useT(historyStrings);
  const c = useT(commonStrings);
  const [filters, setFilters] = useState(NO_FILTERS);
  const [selectedRev, setSelectedRev] = useState<number | null>(focusRev ?? null);
  const h = useHistory(target, filters);

  useEffect(() => {
    if (focusRev != null) setSelectedRev(focusRev);
  }, [focusRev]);

  const selected = h.entries.find((e) => e.rev === selectedRev) ?? null;

  return (
    <div className="grid h-full min-h-0 grid-cols-[minmax(300px,380px)_1fr]">
      <section aria-label={t("revisions")} className="flex min-h-0 flex-col border-r border-line">
        <HistoryFilterBar
          filters={filters}
          authors={h.authors}
          onChange={setFilters}
          scopeLabel={scopeLabel}
          onClearScope={onClearScope}
        />
        <div className="min-h-0 flex-1">
          {h.isPending ? (
            <p className="m-0 p-3 text-muted-text">{t("loadingHistory")}</p>
          ) : h.error ? (
            <EmptyState
              title={t("loadFailed")}
              description={h.error.message}
              actions={<Button onClick={() => h.refetch()}>{c("retry")}</Button>}
            />
          ) : h.entries.length === 0 ? (
            <EmptyState title={t("noRevisions")} description={t("noRevisionsHint")} />
          ) : (
            <VirtualList
              label={t("revisions")}
              className="h-full"
              count={h.entries.length}
              rowHeight={ROW_H}
              onActivate={(i) => {
                const e = h.entries[i];
                if (e) setSelectedRev(e.rev);
              }}
              onActiveChange={(i) => {
                if (
                  h.hasNextPage &&
                  !h.isFetchingNextPage &&
                  i >= h.entries.length - PREFETCH_MARGIN
                ) {
                  void h.fetchNextPage();
                }
              }}
              renderRow={(i, ctx) => {
                const e = h.entries[i];
                if (!e) return null;
                return (
                  <RevisionRow
                    id={ctx.id}
                    active={ctx.active}
                    onPointerActivate={ctx.activate}
                    rev={e.rev}
                    message={e.message}
                    author={e.author ?? c("unknownAuthor")}
                    when={relativeTime(e.date)}
                    selected={e.rev === selectedRev}
                    onSelect={setSelectedRev}
                  />
                );
              }}
            />
          )}
        </div>
        {h.hasNextPage && (
          <div className="border-t border-line p-2">
            <Button
              className="w-full"
              disabled={h.isFetchingNextPage}
              onClick={() => void h.fetchNextPage()}
            >
              {h.isFetchingNextPage ? t("loadingMore") : t("loadMore")}
            </Button>
          </div>
        )}
      </section>
      <section aria-label={t("title")} className="min-h-0 min-w-0">
        <RevisionDetails entry={selected} repoRootUrl={repoRootUrl} nav={nav} />
      </section>
    </div>
  );
}
