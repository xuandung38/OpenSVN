import { useEffect, useRef, useState } from "react";
import { Button } from "../../components/ui/button";
import { useToast } from "../../components/ui/toast";
import { ipc, toAppError, unwrap } from "../../ipc/client";
import type { OpState } from "../../ipc/events";
import { useT } from "../../lib/i18n/i18n";
import { activityStrings } from "./activity-strings";

/** Whether the operation waits for a lost connection. */
export function isWaitingForConnection(op: OpState): boolean {
  return op.state === "running" && op.progress?.stage === "waiting-for-connection";
}

/** Seconds until the next attempt, counted down every second; 0 once it is due. */
function useSecondsUntil(at: number | null): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (at === null) return;
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [at]);
  return at === null ? 0 : Math.max(0, Math.ceil((at - now) / 1000));
}

/**
 * What the live region says: "Connection lost" while the operation waits, "Reconnected" once a
 * wait ended and the operation went on (running or done), nothing otherwise. It changes only when
 * that state changes, never with the countdown.
 */
function useConnectionAnnouncement(op: OpState): "lost" | "reconnected" | null {
  const waiting = isWaitingForConnection(op);
  const waited = useRef(false);
  useEffect(() => {
    if (waiting) waited.current = true;
  }, [waiting]);
  if (waiting) return "lost";
  return waited.current && op.state !== "failed" ? "reconnected" : null;
}

/**
 * "Connection lost, retrying in Ns…" for an operation that waits for the network, with "Retry
 * now" (an immediate attempt) and "Cancel" (the working copy stays incomplete and can be
 * continued later). The visible notice renders only while the operation waits.
 *
 * Screen readers hear a separate live region, kept in the document for as long as the component
 * is mounted so that a change is announced: "Connection lost" when the wait starts and
 * "Reconnected" when it ends. The countdown is ordinary text outside it, so it is not re-read
 * every second.
 */
export function ConnectionWaitNotice({
  op,
  onCancel,
}: {
  op: OpState;
  /** Cancels the operation; omit to hide the button. */
  onCancel?: () => void;
}) {
  const t = useT(activityStrings);
  const toast = useToast();
  const waiting = isWaitingForConnection(op);
  const seconds = useSecondsUntil(waiting ? op.retryAt : null);
  const announcement = useConnectionAnnouncement(op);
  const text = seconds > 0 ? t("waitRetryIn", { seconds }) : t("waitRetrying");
  return (
    <>
      <span
        aria-live="polite"
        aria-atomic="true"
        className="sr-only"
        data-testid="connection-wait-live"
      >
        {announcement === "lost"
          ? t("waitLost")
          : announcement === "reconnected"
            ? t("waitReconnected")
            : ""}
      </span>
      {waiting && (
        <div
          className="flex flex-wrap items-center gap-2 bg-warn-soft px-3.5 py-1.5"
          data-testid="connection-wait"
        >
          <span className="flex-1" data-testid="connection-wait-countdown">
            {text}
          </span>
          <Button
            onClick={() =>
              void (async () => {
                try {
                  await unwrap(ipc().commands.opRetryNow(op.handle.opId));
                } catch (e) {
                  toast.show(toAppError(e).message);
                }
              })()
            }
          >
            {t("waitRetryNow")}
          </Button>
          {onCancel && <Button onClick={onCancel}>{t("cancel")}</Button>}
        </div>
      )}
    </>
  );
}
