import { useEffect, useRef, useState } from "react";
import { VirtualList } from "../../components/list/virtual-list";
import { Button } from "../../components/ui/button";
import { ModalDialog } from "../../components/ui/modal-dialog";
import { SafeText } from "../../components/ui/safe-text";
import type { OpState } from "../../ipc/events";
import { cn } from "../../lib/cn";
import { formatBytes } from "../../lib/format";
import { useT } from "../../lib/i18n/i18n";
import { activityStrings } from "./activity-strings";
import { errorText } from "./error-text";
import {
  ACTION_LETTER,
  countRows,
  finalRevision,
  formatElapsed,
  logText,
  stageKey,
} from "./op-log";

const ROW_HEIGHT = 20;

/** Wall clock that ticks every second while `running` (elapsed time). */
function useNow(running: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!running) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [running]);
  return now;
}

/**
 * Details of one operation, like TortoiseSVN's progress dialog: stage, current path, counts,
 * bytes and rate, elapsed time and a virtualised log of the most recent paths with svn's action
 * letters. When the operation ends, the log stays and a summary (counts, warnings, final
 * revision or the error) appears, with "Copy log" and "Open folder".
 */
export function OpDetails({
  op,
  onClose,
  onCopy,
  onOpenFolder,
}: {
  op: OpState;
  onClose: () => void;
  /** Copies the given text; resolves when done. */
  onCopy: (text: string) => Promise<void> | void;
  /** Shows the working copy folder in the file manager; omitted when there is none. */
  onOpenFolder?: () => void;
}) {
  const t = useT(activityStrings);
  const running = op.state === "running";
  const now = useNow(running);
  const p = op.progress;
  const [copied, setCopied] = useState(false);
  const stage = t(stageKey(op));
  const elapsed = formatElapsed((op.endedAt ?? now) - op.startedAt);
  const files =
    p?.current != null
      ? p.total
        ? t("filesOf", { done: p.current, total: p.total })
        : t("filesCount", { done: p.current })
      : "–";
  const counts = countRows(p?.counts ?? null);
  const rev = finalRevision(op.result);
  const err = op.appError ? errorText(op.appError) : null;

  // Follow the newest path while the user has not scrolled up.
  const scroller = useRef<HTMLDivElement | null>(null);
  const follow = useRef(true);
  useEffect(() => {
    const el = scroller.current;
    if (el && follow.current && op.log.length > 0) el.scrollTop = el.scrollHeight;
  }, [op.log]);

  const status = running
    ? t("statusRunning", { title: op.handle.title, stage })
    : op.state === "done"
      ? t("statusDone", { title: op.handle.title })
      : op.appError?.code === "cancelled"
        ? t("statusCancelled", { title: op.handle.title })
        : t("statusFailed", {
            title: op.handle.title,
            message: err?.key ? t(err.key) : (err?.message ?? ""),
          });

  return (
    <ModalDialog
      open
      onClose={onClose}
      label={t("detailsTitle", { title: op.handle.title })}
      className="w-[min(680px,calc(100%-36px))] rounded-[10px] open:grid open:max-h-[80vh] open:grid-rows-[auto_auto_1fr_auto]"
    >
      <header className="border-b border-line px-4 py-3 text-[15px] font-semibold">
        <SafeText>{op.handle.title}</SafeText>
      </header>
      <div className="px-4 py-2">
        {/* Announced politely: stage changes and the end, not every path. */}
        <p role="status" aria-live="polite" className="sr-only">
          {status}
        </p>
        <dl className="m-0 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1">
          <dt className="text-muted-text">{t("stage")}</dt>
          <dd className="m-0" data-testid="op-stage">
            {stage}
          </dd>
          <dt className="text-muted-text">{t("currentPath")}</dt>
          <dd className="m-0 min-w-0 truncate font-mono text-[12px]">
            <SafeText>{p?.currentPath || "–"}</SafeText>
          </dd>
          <dt className="text-muted-text">{t("files")}</dt>
          <dd className="tnum m-0">{files}</dd>
          <dt className="text-muted-text">{t("received")}</dt>
          <dd className="tnum m-0">{p?.bytesDone != null ? formatBytes(p.bytesDone) : "–"}</dd>
          <dt className="text-muted-text">{t("rate")}</dt>
          <dd className="tnum m-0">
            {running && op.rate !== null ? `${formatBytes(Math.round(op.rate))}/s` : "–"}
          </dd>
          <dt className="text-muted-text">{t("elapsed")}</dt>
          <dd className="tnum m-0">{elapsed}</dd>
        </dl>
        {!running && (
          <section aria-label={t("summary")} className="mt-2 border-t border-line pt-2">
            <h3 className="m-0 mb-1 text-[13px] font-semibold">{t("summary")}</h3>
            {err && (
              <div className="text-bad-text">
                <SafeText className="block break-words">
                  {err.key ? t(err.key) : err.message}
                </SafeText>
                {err.detail && (
                  <details className="mt-1 text-muted-text">
                    <summary className="cursor-pointer">{t("errorDetails")}</summary>
                    <pre className="m-0 mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px]">
                      {err.detail}
                    </pre>
                  </details>
                )}
              </div>
            )}
            {counts.length > 0 && (
              <ul className="m-0 flex list-none flex-wrap gap-x-4 p-0" data-testid="op-counts">
                {counts.map(([key, n]) => (
                  <li key={key}>
                    {t(key)}: <span className="tnum">{n}</span>
                  </li>
                ))}
              </ul>
            )}
            {rev !== null && <p className="m-0">{t("summaryRevision", { rev })}</p>}
            {op.warnings.length > 0 && (
              <div className="mt-1">
                <b className="text-warn-text">{t("warnings")}</b>
                <ul className="m-0 pl-5">
                  {op.warnings.map((w, i) => (
                    // Warnings never reorder: the index is a stable key.
                    // biome-ignore lint/suspicious/noArrayIndexKey: append-only list
                    <li key={i}>
                      <SafeText>{w}</SafeText>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </section>
        )}
      </div>
      <div className="flex min-h-0 flex-col border-t border-line">
        <h3 className="m-0 px-4 pt-2 text-[13px] font-semibold">{t("log")}</h3>
        {op.logDropped > 0 && (
          <p className="m-0 px-4 text-muted-text">{t("logTruncated", { count: op.log.length })}</p>
        )}
        {op.log.length === 0 ? (
          <p className="m-0 px-4 py-2 text-muted-text">{t("logEmpty")}</p>
        ) : (
          <div className="min-h-[160px] flex-1">
            <VirtualList
              count={op.log.length}
              rowHeight={ROW_HEIGHT}
              label={t("log")}
              className="h-[220px] font-mono text-[12px]"
              scrollRef={(node) => {
                scroller.current = node;
                if (node) {
                  node.onscroll = () => {
                    follow.current =
                      node.scrollTop + node.clientHeight >= node.scrollHeight - ROW_HEIGHT * 2;
                  };
                }
              }}
              renderRow={(i, ctx) => {
                const line = op.log[i];
                if (!line) return null;
                return (
                  // biome-ignore lint/a11y/useFocusableInteractive: focus stays on the listbox (aria-activedescendant)
                  // biome-ignore lint/a11y/useKeyWithClickEvents: keyboard moves through the listbox (arrows, Home/End)
                  <div
                    id={ctx.id}
                    role="option"
                    aria-selected={ctx.active}
                    onClick={ctx.activate}
                    className={cn(
                      "flex h-5 items-center gap-3 px-4",
                      ctx.active &&
                        "group-focus-visible:outline-2 group-focus-visible:-outline-offset-2 group-focus-visible:outline-accent",
                    )}
                  >
                    <span className="w-3 text-accent-text">{ACTION_LETTER[line.action]}</span>
                    <SafeText className="truncate">{line.path || "."}</SafeText>
                  </div>
                );
              }}
            />
          </div>
        )}
      </div>
      <footer className="flex justify-end gap-2 border-t border-line bg-surface-2 px-4 py-2.5">
        <Button
          onClick={() =>
            void (async () => {
              await onCopy(logText(op, t));
              setCopied(true);
            })()
          }
        >
          {copied ? t("copied") : t("copyLog")}
        </Button>
        {onOpenFolder && <Button onClick={onOpenFolder}>{t("openFolder")}</Button>}
        <Button onClick={onClose}>{t("close")}</Button>
      </footer>
    </ModalDialog>
  );
}
