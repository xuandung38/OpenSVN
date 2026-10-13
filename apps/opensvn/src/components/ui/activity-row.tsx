import { cn } from "../../lib/cn";
import { SafeText } from "./safe-text";

export interface ActivityRowProps {
  title: string;
  /** 0-100, or null for indeterminate/finished-without-percent. */
  percent: number | null;
  state: "running" | "done" | "failed";
  onCancel?: () => void;
  /** Localised label of the cancel button (defaults to "Cancel"). */
  cancelLabel?: string;
}

const ICON = { running: "●", done: "✓", failed: "✕" } as const;

/** Activity panel row: grid `20px 1fr 160px 70px` (icon, description, progress, percent). */
export function ActivityRow({
  title,
  percent,
  state,
  onCancel,
  cancelLabel = "Cancel",
}: ActivityRowProps) {
  const failed = state === "failed";
  const pct = percent === null ? null : Math.max(0, Math.min(100, Math.round(percent)));
  return (
    <div
      className={cn(
        "grid grid-cols-[20px_1fr_160px_70px] items-center gap-2 px-3.5 py-1",
        failed && "text-bad-text",
      )}
    >
      <span aria-hidden="true" className="font-mono">
        {ICON[state]}
      </span>
      <SafeText className="truncate">{title}</SafeText>
      <div
        role="progressbar"
        aria-label={title}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct ?? undefined}
        className="h-1.5 overflow-hidden rounded-full bg-line"
      >
        <div
          className="h-full bg-accent"
          // Width is data-driven; set via CSSOM (allowed under style-src 'self').
          style={{ width: `${pct ?? 0}%` }}
        />
      </div>
      <span className="tnum flex items-center justify-end gap-1.5 text-muted-text">
        {pct === null ? "" : `${pct}%`}
        {state === "running" && onCancel && (
          <button
            type="button"
            onClick={onCancel}
            className="cursor-pointer border-0 bg-transparent text-accent-text"
          >
            {cancelLabel}
          </button>
        )}
      </span>
    </div>
  );
}
