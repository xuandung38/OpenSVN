import { useEffect, useState } from "react";
import { RevisionRow } from "../../../components/list/revision-row";
import { VirtualList } from "../../../components/list/virtual-list";
import { Button } from "../../../components/ui/button";
import { EmptyState } from "../../../components/ui/empty-state";
import type { LogTarget } from "../../../ipc/bindings";
import { relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { CompareDialog } from "./compare-dialog";
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
  // A comparison of two revisions: the marked one and the selected one.
  const [compareRev, setCompareRev] = useState<number | null>(null);
  const [comparing, setComparing] = useState(false);
  const toggleCompare = (rev: number) => setCompareRev((cur) => (cur === rev ? null : rev));
  const h = useHistory(target, filters);

  useEffect(() => {
    if (focusRev != null) setSelectedRev(focusRev);
  }, [focusRev]);

  const selectedRow = h.rows.find((r) => r.entry.rev === selectedRev) ?? null;
  const selected = selectedRow?.entry ?? null;
  const pair =
    compareRev !== null && selectedRev !== null && compareRev !== selectedRev
      ? { from: Math.min(compareRev, selectedRev), to: Math.max(compareRev, selectedRev) }
      : null;

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
              count={h.rows.length}
              rowHeight={ROW_H}
              onActivate={(i) => {
                const e = h.rows[i]?.entry;
                if (e) setSelectedRev(e.rev);
              }}
              onToggle={(i) => {
                const r = h.rows[i];
                // Only the node's own revisions can be compared.
                if (r && r.depth === 0) toggleCompare(r.entry.rev);
              }}
              onActiveChange={(i) => {
                if (
                  h.hasNextPage &&
                  !h.isFetchingNextPage &&
                  i >= h.rows.length - PREFETCH_MARGIN
                ) {
                  void h.fetchNextPage();
                }
              }}
              renderRow={(i, ctx) => {
                const row = h.rows[i];
                if (!row) return null;
                const e = row.entry;
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
                    badge={row.depth === 0 && e.rev === compareRev ? t("compareBadge") : undefined}
                    indent={row.depth}
                    note={row.mergedBy !== null ? t("mergedIn", { rev: row.mergedBy }) : undefined}
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
      <section aria-label={t("title")} className="flex min-h-0 min-w-0 flex-col">
        {compareRev !== null && (
          <div className="flex flex-wrap items-center gap-2 border-b border-line px-3.5 py-2">
            <p className="m-0 min-w-0 flex-1" role="status">
              {t("compareMarked", { rev: compareRev })} {pair ? null : t("comparePickOther")}
            </p>
            {pair && (
              <Button small variant="primary" onClick={() => setComparing(true)}>
                {t("compareReady", pair)}
              </Button>
            )}
            <Button small onClick={() => setCompareRev(null)}>
              {t("compareClear")}
            </Button>
          </div>
        )}
        <div className="min-h-0 flex-1">
          <RevisionDetails
            entry={selected}
            target={target}
            merged={(selectedRow?.depth ?? 0) > 0}
            repoRootUrl={repoRootUrl}
            nav={nav}
            compareRev={compareRev}
            onMarkCompare={(selectedRow?.depth ?? 0) > 0 ? undefined : toggleCompare}
          />
        </div>
      </section>
      {comparing && pair && (
        <CompareDialog
          target={target}
          from={pair.from}
          to={pair.to}
          repoRootUrl={repoRootUrl}
          onClose={() => setComparing(false)}
        />
      )}
    </div>
  );
}
