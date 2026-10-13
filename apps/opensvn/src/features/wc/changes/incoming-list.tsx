import { useQuery } from "@tanstack/react-query";
import { RevisionRow } from "../../../components/list/revision-row";
import { VirtualList } from "../../../components/list/virtual-list";
import { Button } from "../../../components/ui/button";
import { EmptyState } from "../../../components/ui/empty-state";
import { SafeText } from "../../../components/ui/safe-text";
import { ipc, unwrap } from "../../../ipc/client";
import { queryKeys } from "../../../ipc/query-keys";
import { relativeTime } from "../../../lib/format";
import { commonStrings } from "../../../lib/i18n/common-strings";
import { useT } from "../../../lib/i18n/i18n";
import { useWcActions } from "../use-wc-actions";
import { useWcSession } from "../wc-session";
import { wcStrings } from "../wc-strings";

const REV_H = 44;
const MAX_LISTED = 100;

/** Revisions on the server that the working copy does not have yet, with Update actions. */
export function IncomingList() {
  const { wc, openDialog } = useWcSession();
  const t = useT(wcStrings);
  const c = useT(commonStrings);
  const actions = useWcActions();
  const count = wc?.incoming ?? 0;
  const q = useQuery({
    queryKey: [...queryKeys.logRoot, "incoming", wc?.id ?? "", wc?.baseRev ?? 0, count],
    enabled: wc !== null && count > 0,
    queryFn: () =>
      unwrap(
        ipc().commands.logGet({
          target: { kind: "url", url: (wc as NonNullable<typeof wc>).url },
          fromRev: null,
          toRev: (wc as NonNullable<typeof wc>).baseRev + 1,
          limit: Math.min(count, MAX_LISTED),
          stopOnCopy: null,
          withPaths: false,
          filter: null,
        }),
      ),
  });

  if (count === 0) {
    return (
      <EmptyState
        title={t("incomingTitle")}
        description={t("incomingHint")}
        actions={
          <Button onClick={() => void actions.checkForUpdates()}>{t("checkForUpdates")}</Button>
        }
      />
    );
  }
  const entries = q.data?.entries ?? [];
  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-2.5 py-2">
        <span className="flex-1 font-semibold">{t("incomingCount", { count })}</span>
        <Button small variant="primary" onClick={() => void actions.update(null)}>
          {t("updateAll")}
        </Button>
        <Button small onClick={() => openDialog({ kind: "update-rev" })}>
          {t("updateToRevision")}
        </Button>
      </div>
      {q.isPending ? (
        <p className="m-0 p-3 text-muted-text">{c("loading")}</p>
      ) : q.isError ? (
        <SafeText as="p" className="m-0 p-3 text-bad-text">
          {q.error.message}
        </SafeText>
      ) : (
        <VirtualList
          label={t("tabIncoming")}
          className="min-h-0 flex-1"
          count={entries.length}
          rowHeight={REV_H}
          renderRow={(i, ctx) => {
            const e = entries[i];
            return e ? (
              <RevisionRow
                id={ctx.id}
                active={ctx.active}
                onPointerActivate={ctx.activate}
                incoming
                rev={e.rev}
                message={e.message}
                author={e.author ?? c("unknownAuthor")}
                when={relativeTime(e.date)}
              />
            ) : null;
          }}
        />
      )}
    </div>
  );
}
