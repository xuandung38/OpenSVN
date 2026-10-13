import type { OpState } from "../../ipc/events";
import { ActivityRow } from "../ui/activity-row";

/** Collapsible panel under the main area (max 150-200px) listing running/finished operations. */
export function ActivityPanel({
  ops,
  onCancel,
  cancelLabel,
}: {
  ops: OpState[];
  /** Cancel a running operation; omit to hide the button. */
  onCancel?: (opId: string) => void;
  cancelLabel?: string;
}) {
  return (
    <section
      aria-label="Activity"
      className="max-h-[180px] overflow-auto border-t border-line bg-surface"
    >
      {ops.length === 0 && <p className="m-0 px-3.5 py-2 text-muted-text">No recent activity.</p>}
      {ops.map((o) => {
        const pr = o.progress;
        const percent = pr?.current != null && pr.total ? (pr.current / pr.total) * 100 : null;
        return (
          <ActivityRow
            key={o.handle.opId}
            title={o.error ? `${o.handle.title}: ${o.error}` : o.handle.title}
            percent={o.state === "done" ? 100 : percent}
            state={o.state}
            onCancel={onCancel ? () => onCancel(o.handle.opId) : undefined}
            cancelLabel={cancelLabel}
          />
        );
      })}
    </section>
  );
}
