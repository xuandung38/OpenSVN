import { useId, useState } from "react";
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
  /** Opens the operation's details; the title becomes a button. */
  onOpen?: () => void;
  /** Accessible name of the open button (defaults to the title). */
  openLabel?: string;
  /** Short status text after the title (a failure's localised message). */
  message?: string | null;
  /** Technical text shown behind a "Details" disclosure when the row is expanded. */
  detail?: string | null;
  /** Label of the disclosure (defaults to "Details"). */
  detailLabel?: string;
}

const ICON = { running: "●", done: "✓", failed: "✕" } as const;

/**
 * Activity panel row: grid `20px 1fr 160px 70px` (icon, description, progress, percent). A row
 * with a message can be expanded (chevron) to show it in full, plus its technical detail.
 */
export function ActivityRow({
  title,
  percent,
  state,
  onCancel,
  cancelLabel = "Cancel",
  onOpen,
  openLabel,
  message,
  detail,
  detailLabel = "Details",
}: ActivityRowProps) {
  const failed = state === "failed";
  const pct = percent === null ? null : Math.max(0, Math.min(100, Math.round(percent)));
  const [expanded, setExpanded] = useState(false);
  const moreId = useId();
  const expandable = Boolean(message || detail);
  const label = message ? `${title}: ${message}` : title;
  return (
    <div className={cn("px-3.5 py-1", failed && "text-bad-text")}>
      <div className="grid grid-cols-[20px_1fr_160px_70px] items-center gap-2">
        {expandable ? (
          <button
            type="button"
            aria-expanded={expanded}
            aria-controls={moreId}
            aria-label={`${detailLabel}: ${title}`}
            onClick={() => setExpanded((v) => !v)}
            className="cursor-pointer border-0 bg-transparent p-0 font-mono text-inherit"
          >
            {expanded ? "▾" : ICON[state]}
          </button>
        ) : (
          <span aria-hidden="true" className="font-mono">
            {ICON[state]}
          </span>
        )}
        {onOpen ? (
          <button
            type="button"
            onClick={onOpen}
            aria-label={openLabel}
            className="min-w-0 cursor-pointer truncate border-0 bg-transparent p-0 text-left text-inherit hover:underline focus-visible:underline"
          >
            <SafeText>{label}</SafeText>
          </button>
        ) : (
          <SafeText className="truncate">{label}</SafeText>
        )}
        <div
          role="progressbar"
          aria-label={title}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={pct ?? undefined}
          className="h-1.5 overflow-hidden rounded-full bg-line"
        >
          {pct === null && state === "running" ? (
            // No total known (e.g. a checkout that does not count its items first): indeterminate.
            <div
              data-indeterminate=""
              className="h-full w-1/3 bg-accent motion-safe:animate-pulse"
            />
          ) : (
            <div
              className="h-full bg-accent"
              // Width is data-driven; set via CSSOM (allowed under style-src 'self').
              style={{ width: `${pct ?? 0}%` }}
            />
          )}
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
      {expandable && expanded && (
        <div id={moreId} className="ml-7 py-1 text-fg">
          {message && <SafeText className="block break-words">{message}</SafeText>}
          {detail && (
            <details className="mt-1">
              <summary className="cursor-pointer text-muted-text">{detailLabel}</summary>
              <pre className="m-0 mt-1 max-h-32 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] text-muted-text">
                {detail}
              </pre>
            </details>
          )}
        </div>
      )}
    </div>
  );
}
