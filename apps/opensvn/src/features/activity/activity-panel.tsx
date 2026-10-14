import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ActivityRow } from "../../components/ui/activity-row";
import { SafeText } from "../../components/ui/safe-text";
import { useToast } from "../../components/ui/toast";
import { ipc, toAppError, unwrap } from "../../ipc/client";
import { type OpState, removeOp } from "../../ipc/events";
import { useT } from "../../lib/i18n/i18n";
import { activityStrings } from "./activity-strings";
import { errorText } from "./error-text";
import { OpDetails } from "./op-details";
import { stageKey } from "./op-log";

/** Working copy whose folder "Open folder" shows: the operation's own, or the one a checkout made. */
function folderWc(op: OpState): string | null {
  if (op.handle.wcId) return op.handle.wcId;
  if (op.result?.kind === "checkedOut") return op.result.wcId;
  return null;
}

/**
 * Collapsible panel under the main area (max 150-200px) listing running/finished operations.
 * Each row opens the operation's details (stage, paths, bytes, log, summary). Operations the app
 * started itself (periodic update checks) get no row: a failure shows as one small indicator.
 */
export function ActivityPanel({
  ops,
  onCancel,
}: {
  ops: OpState[];
  /** Cancel a running operation; omit to hide the button. */
  onCancel?: (opId: string) => void;
}) {
  const t = useT(activityStrings);
  const qc = useQueryClient();
  const toast = useToast();
  const [openId, setOpenId] = useState<string | null>(null);
  const [showBackground, setShowBackground] = useState(false);
  const visible = ops.filter((o) => !o.handle.background);
  const backgroundFailed = ops.filter((o) => o.handle.background && o.state === "failed");
  const open = ops.find((o) => o.handle.opId === openId) ?? null;
  const openWc = open ? folderWc(open) : null;
  // Screen readers hear the newest operation's stage and outcome, not every progress tick.
  const latest = visible.at(-1);
  const announce = (() => {
    if (!latest) return "";
    const title = latest.handle.title;
    if (latest.state === "running")
      return t("statusRunning", { title, stage: t(stageKey(latest)) });
    if (latest.state === "done") return t("statusDone", { title });
    if (latest.appError?.code === "cancelled") return t("statusCancelled", { title });
    const err = latest.appError ? errorText(latest.appError) : null;
    return t("statusFailed", {
      title,
      message: err?.key ? t(err.key) : (err?.message ?? latest.error ?? ""),
    });
  })();

  return (
    <section
      aria-label={t("region")}
      className="max-h-[180px] overflow-auto border-t border-line bg-surface"
    >
      <p role="status" aria-live="polite" className="sr-only">
        {announce}
      </p>
      {visible.length === 0 && backgroundFailed.length === 0 && (
        <p className="m-0 px-3.5 py-2 text-muted-text">{t("empty")}</p>
      )}
      {visible.map((o) => {
        const pr = o.progress;
        const percent = pr?.current != null && pr.total ? (pr.current / pr.total) * 100 : null;
        const err = o.appError ? errorText(o.appError) : null;
        const message = err ? (err.key ? t(err.key) : err.message) : o.error;
        return (
          <ActivityRow
            key={o.handle.opId}
            title={o.handle.title}
            message={message}
            detail={err?.detail ?? null}
            detailLabel={t("errorDetails")}
            percent={o.state === "done" ? 100 : percent}
            state={o.state}
            onOpen={() => setOpenId(o.handle.opId)}
            openLabel={t("showDetails", { title: o.handle.title })}
            onCancel={onCancel ? () => onCancel(o.handle.opId) : undefined}
            cancelLabel={t("cancel")}
          />
        );
      })}
      {backgroundFailed.length > 0 && (
        <div
          className="flex items-center gap-2 px-3.5 py-1 text-muted-text"
          data-testid="background-failures"
        >
          <button
            type="button"
            aria-expanded={showBackground}
            onClick={() => setShowBackground((v) => !v)}
            className="cursor-pointer border-0 bg-transparent p-0 text-left text-muted-text hover:underline"
          >
            <span aria-hidden="true">⚠ </span>
            {backgroundFailed.length === 1
              ? t("backgroundFailed")
              : t("backgroundFailedMany", { count: backgroundFailed.length })}
          </button>
          <span className="flex-1" />
          <button
            type="button"
            onClick={() => {
              for (const o of backgroundFailed) removeOp(qc, o.handle.opId);
              setShowBackground(false);
            }}
            className="cursor-pointer border-0 bg-transparent text-accent-text"
          >
            {t("backgroundDismiss")}
          </button>
        </div>
      )}
      {showBackground &&
        backgroundFailed.map((o) => {
          const err = o.appError ? errorText(o.appError) : null;
          return (
            <p key={o.handle.opId} className="m-0 px-3.5 pl-9 text-muted-text">
              <SafeText>
                {o.handle.title}: {err?.key ? t(err.key) : (err?.message ?? o.error ?? "")}
              </SafeText>
            </p>
          );
        })}
      {open && (
        <OpDetails
          op={open}
          onClose={() => setOpenId(null)}
          onCopy={async (text) => {
            try {
              await navigator.clipboard.writeText(text);
            } catch (e) {
              toast.show(toAppError(e).message);
            }
          }}
          onOpenFolder={
            openWc
              ? () =>
                  void (async () => {
                    try {
                      await unwrap(ipc().commands.revealInOs(openWc, ""));
                    } catch (e) {
                      toast.show(toAppError(e).message, 6000);
                    }
                  })()
              : undefined
          }
        />
      )}
    </section>
  );
}
