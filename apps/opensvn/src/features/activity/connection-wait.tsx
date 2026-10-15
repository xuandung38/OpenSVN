import { useEffect, useState } from "react";
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
 * "Connection lost, retrying in Ns…" for an operation that waits for the network, with "Retry
 * now" (an immediate attempt) and "Cancel" (the working copy stays incomplete and can be
 * continued later). Renders nothing when the operation is not waiting.
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
  const seconds = useSecondsUntil(op.retryAt);
  if (!isWaitingForConnection(op)) return null;
  const text = seconds > 0 ? t("waitRetryIn", { seconds }) : t("waitRetrying");
  return (
    <div
      className="flex flex-wrap items-center gap-2 bg-warn-soft px-3.5 py-1.5"
      data-testid="connection-wait"
    >
      <span className="flex-1" role="status">
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
  );
}
